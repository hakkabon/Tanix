//! Tanix Phase-20 FAT16 filesystem — a small, no_std, block-agnostic
//! FAT16 reader/writer.
//!
//! The volume layout is read from the boot sector (BPB) at runtime, so any
//! consistent FAT16 image works (the host-side `scripts/mkfat16.py` builds
//! the demo volume).  Geometry:
//!
//!   • sector size 512 B;
//!   • the whole FAT (typically ≪ 4 KiB) is cached in one static buffer at
//!     mount time — chain walks and cluster allocation run in memory;
//!   • the root directory is the fixed-size FAT16 root (up to `root_entries`
//!     × 32-byte slots) right after the FATs;
//!   • files are 8.3-named (uppercase), written either by overwriting
//!     within their extent or by appending at EOF (allocating and chaining
//!     clusters as needed).
//!
//! The crate is host-testable: `BlockIo` is the only hook, and the image
//! parser/chain walker are pure over it.

#![cfg_attr(not(test), no_std)]

/// A minimal block interface — one sector per call.  Implemented by the
/// servers' virtio-blk driver.
pub trait BlockIo {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8; 512]) -> bool;
    fn write_sector(&mut self, sector: u64, buf: &[u8; 512]) -> bool;
}

/// The number of clusters the FAT16 cache can hold entries for.
pub const MAX_CACHED_CLUSTERS: usize = 4096;
/// Maximum root-directory entries.
pub const MAX_ROOT_ENTRIES: usize = 512;

/// 8.3 short-name as stored in a directory entry.
pub type ShortName = [u8; 11];

/// Fat semantics for the cached FAT.  Cluster numbers are u16 with
/// 0xFFF8..=0xFFFF == end-of-chain.
const FAT_EOC: u16 = 0xFFFF;
const FAT_FREE: u16 = 0x0000;

/// Root-directory entry flags.
pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;

/// One root-directory entry (32 bytes, little-endian fields).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name: ShortName, // 8.3, uppercase, space-padded
    pub attr: u8,
    _nt_res: u8,
    pub crt_tenth: u8,
    pub crt_time: u16,
    pub crt_date: u16,
    pub last_acc_date: u16,
    _hi_start: u16,
    pub wrt_time: u16,
    pub wrt_date: u16,
    pub start_cluster: u16,
    pub size: u32,
}

impl DirEntry {
    pub const EMPTY: DirEntry = DirEntry {
        name: [0; 11],
        attr: 0,
        _nt_res: 0,
        crt_tenth: 0,
        crt_time: 0,
        crt_date: 0,
        last_acc_date: 0,
        _hi_start: 0,
        wrt_time: 0,
        wrt_date: 0,
        start_cluster: 0,
        size: 0,
    };

    pub fn is_deleted(&self) -> bool {
        self.name[0] == 0xE5
    }

    pub fn is_empty(&self) -> bool {
        self.name[0] == 0x00
    }

    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }

    pub fn is_volume(&self) -> bool {
        self.attr & (ATTR_VOLUME_ID | ATTR_DIRECTORY) == ATTR_VOLUME_ID
    }
}

/// The little-endian helpers the server needs for the wire format.
impl DirEntry {
    /// Encode the 8.3 name as two u32 words (little-endian byte order).
    pub fn name_words(&self) -> (u32, u32) {
        (u32::from_le_bytes([self.name[0], self.name[1], self.name[2], self.name[3]]),
         u32::from_le_bytes([self.name[4], self.name[5], self.name[6], self.name[7]]))
    }
}

/// A mounted FAT16 volume.
pub struct Fat16 {
    /// Bytes per sector (must be 512).
    pub sector_size: u16,
    /// Sectors per cluster.
    pub sectors_per_cluster: u8,
    /// Absolute sector of the first FAT.
    pub fat_base: u64,
    /// Sectors occupied by one FAT (we cache the first copy).
    pub fat_sectors: u16,
    /// Absolute sector of the root directory.
    pub root_base: u64,
    /// Root directory entries.
    pub root_entries: u16,
    /// Absolute sector of the data area (cluster 2 starts here).
    pub data_base: u64,
    /// Total clusters on the volume.
    pub total_clusters: u16,
    /// Cluster size in bytes.
    pub cluster_bytes: u32,
    /// The cached FAT (first copy, little-endian u16 entries; index =
    /// cluster number).
    pub fat: [u8; MAX_CACHED_CLUSTERS * 2],
    /// Number of valid entries in `fat`.
    pub fat_entries: u16,
}

impl Fat16 {
    /// Read the boot sector and initialise the volume structure.
    pub fn mount(blk: &mut dyn BlockIo) -> Option<Fat16> {
        let mut bs = [0u8; 512];
        if !blk.read_sector(0, &mut bs) {
            return None;
        }
        if bs[510] != 0x55 || bs[511] != 0xAA {
            return None; // no boot signature
        }
        let sector_size = u16::from_le_bytes([bs[0x0B], bs[0x0C]]);
        if sector_size != 512 {
            return None;
        }
        let sectors_per_cluster = bs[0x0D];
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return None;
        }
        let reserved = u16::from_le_bytes([bs[0x0E], bs[0x0F]]);
        let num_fats = bs[0x10];
        let root_entries = u16::from_le_bytes([bs[0x11], bs[0x12]]);
        let total_sectors = u16::from_le_bytes([bs[0x13], bs[0x14]]);
        let media = bs[0x15];
        let fat_sectors = u16::from_le_bytes([bs[0x16], bs[0x17]]);
        if num_fats == 0 || fat_sectors == 0 || total_sectors == 0 {
            return None;
        }
        // Reject FAT12/16-in-FAT16-form mismatches: real FAT16 volumes use
        // two FATs in practice but a single copy is legal — we only ever
        // touch the first.
        let _ = media;

        let root_sectors = (root_entries as u32 * 32).div_ceil(512) as u64;
        let fat_base = reserved as u64;
        let root_base = fat_base + num_fats as u64 * fat_sectors as u64;
        let data_base = root_base + root_sectors;
        if data_base >= total_sectors as u64 {
            return None;
        }
        let total_clusters =
            ((total_sectors as u64 - data_base) / sectors_per_cluster as u64) as u16;
        if total_clusters < 2 || total_clusters > MAX_CACHED_CLUSTERS as u16 {
            return None;
        }

        let mut fs = Fat16 {
            sector_size,
            sectors_per_cluster,
            fat_base,
            fat_sectors,
            root_base,
            root_entries: root_entries.min(MAX_ROOT_ENTRIES as u16),
            data_base,
            total_clusters,
            cluster_bytes: 512u32 * sectors_per_cluster as u32,
            fat: [0; MAX_CACHED_CLUSTERS * 2],
            fat_entries: total_clusters,
        };

        // Load the whole FAT (the cached portion).
        for i in 0..fat_sectors {
            let mut sec = [0u8; 512];
            if !blk.read_sector(fat_base + i as u64, &mut sec) {
                return None;
            }
            let dst = i as usize * 512;
            let n = (fs.fat.len() - dst).min(512);
            fs.fat[dst..dst + n].copy_from_slice(&sec[..n]);
        }
        Some(fs)
    }

    // ── FAT access (cached) ───────────────────────────────────────────────────

    #[inline]
    fn fat_entry(&self, cluster: u16) -> u16 {
        let i = cluster as usize * 2;
        u16::from_le_bytes([self.fat[i], self.fat[i + 1]])
    }

    fn set_fat_entry(&mut self, cluster: u16, value: u16) {
        let i = cluster as usize * 2;
        self.fat[i] = (value & 0xFF) as u8;
        self.fat[i + 1] = (value >> 8) as u8;
    }

    /// Persist the cached FAT back to the volume.
    pub fn flush_fat(&self, blk: &mut dyn BlockIo) -> bool {
        let n = self.fat_entries as usize * 2;
        for i in 0..self.fat_sectors {
            let mut sec = [0u8; 512];
            let src = i as usize * 512;
            if src < n {
                let take = (n - src).min(512);
                sec[..take].copy_from_slice(&self.fat[src..src + take]);
            }
            if !blk.write_sector(self.fat_base + i as u64, &sec) {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn sector_of(&self, cluster: u16) -> u64 {
        self.data_base + ((cluster as u64 - 2) * self.sectors_per_cluster as u64)
    }

    /// Number of free clusters (FAT entries == 0).
    pub fn free_clusters(&self) -> u16 {
        (2..self.fat_entries).filter(|&c| self.fat_entry(c) == FAT_FREE).count() as u16
    }

    /// Find a free cluster and chain `prev` → it (or end the chain if
    /// `prev` is 0).  Returns the new cluster, or None when the volume is
    /// full.  The caller must `flush_fat` before the changes hit disk.
    pub fn alloc_cluster(&mut self, prev: u16) -> Option<u16> {
        for c in 2..self.fat_entries {
            if self.fat_entry(c) == FAT_FREE {
                self.set_fat_entry(c, FAT_EOC);
                if prev != 0 {
                    self.set_fat_entry(prev, c);
                }
                return Some(c);
            }
        }
        None
    }

    /// Follow the cluster chain one step: `cluster` → its successor (0 =
    /// free, 0xFFF8..=0xFFFF = end of chain).
    pub fn next_cluster(&self, cluster: u16) -> u16 {
        let e = self.fat_entry(cluster);
        if e >= 0xFFF8 || e < 2 {
            return 0; // EOC (or corrupt)
        }
        e
    }

    // ── Root directory ────────────────────────────────────────────────────────

    /// Convert an 8.3 name into its FAT lowercase-stripped form: bytes are
    /// uppercased, spaces are trimmed from the base and extension.
    pub fn short_name(name: &str) -> Option<ShortName> {
        let mut out = [b' '; 11];
        if name.is_empty() || name.len() > 12 {
            return None;
        }
        let mut base = name;
        if let Some(dot) = name.rfind('.') {
            let ext = &name[dot + 1..];
            if ext.len() > 3 {
                return None;
            }
            base = &name[..dot];
            for (i, b) in ext.bytes().enumerate() {
                out[8 + i] = b.to_ascii_uppercase();
            }
        }
        if base.len() > 8 {
            return None;
        }
        for (i, b) in base.bytes().enumerate() {
            if !(b.is_ascii_alphanumeric() || b == b' ' || b == b'_' || b == b'-') {
                return None;
            }
            out[i] = b.to_ascii_uppercase();
        }
        Some(out)
    }

    /// Read the root-directory sector hosting entry `idx`.
    fn read_root_sector(&self, blk: &mut dyn BlockIo, idx: usize, sec: &mut [u8; 512]) -> bool {
        let sect = self.root_base + (idx / 16) as u64; // 16 entries per 512 B
        blk.read_sector(sect, sec)
    }

    /// Find a root entry by 8.3 name.  On success returns `(index, entry)`.
    pub fn find_root(&self, blk: &mut dyn BlockIo, name: &ShortName) -> Option<(usize, DirEntry)> {
        for idx in 0..self.root_entries as usize {
            let mut sec = [0u8; 512];
            if !self.read_root_sector(blk, idx, &mut sec) {
                return None;
            }
            let e: DirEntry = unsafe { core::ptr::read_unaligned(sec.as_ptr().add((idx % 16) * 32).cast()) };
            if e.is_empty() {
                return None; // end of directory
            }
            if !e.is_deleted() && !e.is_volume() && &e.name == name {
                return Some((idx, e));
            }
        }
        None
    }

    // ── Files ─────────────────────────────────────────────────────────────────

    /// Read `out.len()` bytes of the file described by `(first_cluster,
    /// size)` at byte `offset`.  Returns the bytes read (< out.len() at
    /// EOF or when the chain ends early).
    pub fn read_file(
        &self,
        blk: &mut dyn BlockIo,
        first_cluster: u16,
        size: u32,
        offset: u32,
        out: &mut [u8],
    ) -> usize {
        if offset >= size || out.is_empty() {
            return 0;
        }
        let cb = self.cluster_bytes as usize;
        let mut cluster = first_cluster;
        let mut skip = offset as usize;
        // Walk to the covering cluster.
        while cluster >= 2 && skip >= cb {
            skip -= cb;
            cluster = self.next_cluster(cluster);
            if cluster == 0 {
                return 0;
            }
        }
        if cluster < 2 {
            return 0;
        }
        let remain = size.saturating_sub(offset) as usize;
        let mut written = 0usize;
        let mut sec = [0u8; 512];
        let mut sskip = skip; // intra-cluster bytes to drop (first cluster only)
        while cluster >= 2 && written < remain && written < out.len() {
            let sector = self.sector_of(cluster);
            for i in 0..self.sectors_per_cluster as usize {
                if written >= remain || written >= out.len() {
                    break;
                }
                if !blk.read_sector(sector + i as u64, &mut sec) {
                    return written;
                }
                let s = sskip.min(512); // drop this many bytes inside the sector
                let take = (512 - s).min(remain - written).min(out.len() - written);
                if take > 0 {
                    out[written..written + take].copy_from_slice(&sec[s..s + take]);
                    written += take;
                }
                sskip = sskip.saturating_sub(512);
            }
            sskip = 0;
            cluster = self.next_cluster(cluster);
        }
        written
    }

    /// Overwrite or append `data` within the file described by an existing
    /// root entry.  `offset ≤ size` overwrites in place; `offset == size`
    /// appends, allocating + chaining clusters as needed (the FAT and the
    /// root entry are flushed before returning).  Returns the new file
    /// size, or None on error.
    pub fn write_file(
        &mut self,
        blk: &mut dyn BlockIo,
        entry_idx: usize,
        entry: &mut DirEntry,
        offset: u32,
        data: &[u8],
    ) -> Option<u32> {
        if data.is_empty() {
            return Some(entry.size);
        }
        let cb = self.cluster_bytes as usize;
        let mut cluster = entry.start_cluster;
        let mut pos = offset as usize;
        let mut skip = pos;
        let mut src = 0usize;
        let mut sec = [0u8; 512];

        // If the file has no clusters yet, allocate the first one.
        if cluster == 0 {
            cluster = self.alloc_cluster(0)?;
            entry.start_cluster = cluster;
        }

        // Walk to the covering cluster.
        while cluster >= 2 && skip >= cb {
            skip -= cb;
            let next = self.next_cluster(cluster);
            if next == 0 {
                // Extending beyond the chain: allocate.
                cluster = self.alloc_cluster(cluster)?;
            } else {
                cluster = next;
            }
        }
        if cluster < 2 {
            return None;
        }

        let mut sskip = skip; // intra-cluster bytes to skip (first cluster only)

        while src < data.len() {
            let sector = self.sector_of(cluster);
            for i in 0..self.sectors_per_cluster as usize {
                if src == data.len() {
                    break;
                }
                let s = sskip.min(512); // offset inside the first touched sector
                let need = (512 - s).min(data.len() - src);
                // Read-modify-write unless we're overwriting the whole
                // sector in place.
                if s > 0 || need < 512 {
                    if !blk.read_sector(sector + i as u64, &mut sec) {
                        return None;
                    }
                } else {
                    sec = [0u8; 512];
                }
                sec[s..s + need].copy_from_slice(&data[src..src + need]);
                if !blk.write_sector(sector + i as u64, &sec) {
                    return None;
                }
                src += need;
                pos += need;
                sskip = sskip.saturating_sub(512);
            }
            sskip = 0;
            if src < data.len() {
                let next = self.next_cluster(cluster);
                cluster = if next == 0 { self.alloc_cluster(cluster)? } else { next };
            }
        }
        entry.size = entry.size.max(pos as u32);
        // Persist the FAT and the root entry.
        if !self.flush_fat(blk) {
            return None;
        }
        self.write_root_entry(blk, entry_idx, entry);
        Some(entry.size)
    }

    /// Write a root entry back to its slot on disk.
    pub fn write_root_entry(&self, blk: &mut dyn BlockIo, idx: usize, e: &DirEntry) -> bool {
        let mut sec = [0u8; 512];
        if !self.read_root_sector(blk, idx, &mut sec) {
            return false;
        }
        unsafe {
            let p = sec.as_mut_ptr().add((idx % 16) * 32).cast::<DirEntry>();
            ptr_write_unaligned(p, *e);
        }
        let sect = self.root_base + (idx / 16) as u64;
        blk.write_sector(sect, &sec)
    }

    /// Create a new root file entry in the first free slot.  Returns its
    /// index.  `size` starts at 0.
    pub fn create_root_file(
        &mut self,
        blk: &mut dyn BlockIo,
        name: &ShortName,
    ) -> Option<usize> {
        for idx in 0..self.root_entries as usize {
            let mut sec = [0u8; 512];
            if !self.read_root_sector(blk, idx, &mut sec) {
                return None;
            }
            let e: DirEntry = unsafe { core::ptr::read_unaligned(sec.as_ptr().add((idx % 16) * 32).cast()) };
            if e.is_empty() || e.is_deleted() {
                let mut ne = DirEntry::EMPTY;
                ne.name = *name;
                ne.attr = ATTR_ARCHIVE;
                ne.start_cluster = 0;
                ne.size = 0;
                if self.write_root_entry(blk, idx, &ne) {
                    return Some(idx);
                }
                return None;
            }
        }
        None
    }
}

/// `ptr::write_unaligned`-style store (keeps the no_std surface tidy).
#[inline]
unsafe fn ptr_write_unaligned<T: Copy>(p: *mut T, v: T) {
    core::ptr::write_unaligned(p, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny in-memory volume mirroring `scripts/mkfat16.py`: 2048
    /// sectors, 1 reserved, 1 FAT (2 sectors), 16 root entries, cluster =
    /// 4 sectors.
    struct MemBlk {
        sectors: Vec<[u8; 512]>,
    }

    impl MemBlk {
        fn new() -> MemBlk {
            let mut sectors = vec![[0u8; 512]; 2048];
            // Boot sector.
            let bs = &mut sectors[0];
            bs[0x0B] = 0; bs[0x0C] = 2; // bytes/sector = 512
            bs[0x0D] = 4; // sectors/cluster
            bs[0x0E] = 1; bs[0x0F] = 0; // reserved
            bs[0x10] = 1; // 1 FAT
            bs[0x11] = 16; bs[0x12] = 0; // root entries
            bs[0x13] = 0; bs[0x14] = 8; // total sectors = 2048
            bs[0x15] = 0xF8; // media
            bs[0x16] = 2; bs[0x17] = 0; // FAT = 2 sectors
            bs[510] = 0x55; bs[511] = 0xAA;
            MemBlk { sectors }
        }
        fn cluster_chain(&mut self, first: u16, count: u16) {
            // Write FAT entries: first → ... → EOC.
            let mut c = first;
            for i in 0..count {
                let next = if i + 1 == count { 0xFFFF } else { c + 1 };
                let fat_off = c as usize * 2;
                let sec = &mut self.sectors[1 + fat_off / 512];
                let o = fat_off % 512;
                sec[o] = (next & 0xFF) as u8;
                sec[o + 1] = (next >> 8) as u8;
                c = next;
            }
        }
        fn root(&mut self, idx: usize, e: &DirEntry) {
            let sec = &mut self.sectors[1 + 2 + idx / 16];
            unsafe {
                let p = sec.as_mut_ptr().add((idx % 16) * 32).cast::<DirEntry>();
                ptr_write_unaligned(p, *e);
            }
        }
    }

    impl BlockIo for MemBlk {
        fn read_sector(&mut self, sector: u64, buf: &mut [u8; 512]) -> bool {
            buf.copy_from_slice(&self.sectors[sector as usize]);
            true
        }
        fn write_sector(&mut self, sector: u64, buf: &[u8; 512]) -> bool {
            self.sectors[sector as usize].copy_from_slice(buf);
            true
        }
    }

    fn fat16_entry(name: &str, cluster: u16, size: u32) -> DirEntry {
        let mut e = DirEntry::EMPTY;
        e.name = Fat16::short_name(name).unwrap();
        e.attr = ATTR_ARCHIVE;
        e.start_cluster = cluster;
        e.size = size;
        e
    }

    #[test]
    fn mount_and_read_chain() {
        let mut blk = MemBlk::new();
        // File A.TXT: clusters 2,3,4 (3 × 2048 B), size 3000.
        blk.cluster_chain(2, 3);
        for i in 0..3000 {
            let sec = &mut blk.sectors[4 + i / 512]; // data_base=4 (1 res + 2 FAT + 1 root)
            sec[i % 512] = (i % 251) as u8;
        }
        blk.root(0, &fat16_entry("A.TXT", 2, 3000));

        let mut fs = Fat16::mount(&mut blk).unwrap();
        assert_eq!(fs.sectors_per_cluster, 4);
        assert_eq!(fs.data_base, 4);
        assert_eq!(fs.total_clusters, (2048 - 4) / 4);

        // Read the whole file.
        let mut out = [0u8; 64];
        let n = fs.read_file(&mut blk, 2, 3000, 0, &mut out);
        assert_eq!(n, 64);
        for i in 0..64 {
            assert_eq!(out[i], (i % 251) as u8);
        }
        // Read at an offset crossing the cluster boundary.
        let mut out2 = [0u8; 32];
        let n2 = fs.read_file(&mut blk, 2, 3000, 2000, &mut out2);
        assert_eq!(n2, 32);
        assert_eq!(out2[0], (2000 % 251) as u8);
        // EOF.
        let mut out3 = [0u8; 8];
        assert_eq!(fs.read_file(&mut blk, 2, 3000, 3000, &mut out3), 0);
        // find via the root.
        let name = Fat16::short_name("A.TXT").unwrap();
        let (idx, e) = fs.find_root(&mut blk, &name).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(e.size, 3000);

        // Append: extend from 3000 to 3100.
        let mut e = e;
        let payload = [0xAAu8; 100];
        let new_size = fs.write_file(&mut blk, idx, &mut e, 3000, &payload).unwrap();
        assert_eq!(new_size, 3100);
        let mut tail = [0u8; 100];
        fs.read_file(&mut blk, 2, 3100, 3000, &mut tail);
        assert!(tail.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn find_entries_after_root_slot_zero() {
        // Mirrors the real mkfat16 demo volume: entries at indices 0..3
        // in the root directory, found by 8.3 name (regression for the
        // demo volume where only slot 0 ever matched).
        let mut blk = MemBlk::new();
        let spec = [
            ("README.TXT", 2u16, 162u32),
            ("VERSION.TXT", 3, 42),
            ("DATA.BIN", 4, 3000),
            ("VISIT.LOG", 0, 0),
        ];
        for (i, (name, cluster, size)) in spec.iter().enumerate() {
            blk.root(i, &fat16_entry(name, *cluster, *size));
        }
        let mut fs = Fat16::mount(&mut blk).unwrap();
        for (i, (name, cluster, size)) in spec.iter().enumerate() {
            let short = Fat16::short_name(name).unwrap();
            let (idx, e) = fs.find_root(&mut blk, &short).unwrap();
            assert_eq!(idx, i, "expected entry {name} at index {i}");
            assert_eq!(e.start_cluster, *cluster);
            assert_eq!(e.size, *size);
        }
        // A name that is not present must not be found.
        let missing = Fat16::short_name("NOPE.DAT").unwrap();
        assert!(fs.find_root(&mut blk, &missing).is_none());
    }

    #[test]
    fn append_allocates_clusters() {
        let mut blk = MemBlk::new();
        blk.root(0, &fat16_entry("BIG.TXT", 0, 0)); // empty file
        let mut fs = Fat16::mount(&mut blk).unwrap();
        let name = Fat16::short_name("BIG.TXT").unwrap();
        let (idx, mut e) = fs.find_root(&mut blk, &name).unwrap();
        assert_eq!(e.start_cluster, 0);

        // 5000 bytes > 2048 × 2 — must stretch two cluster allocations.
        let payload = [0x7Eu8; 5000];
        let size = fs.write_file(&mut blk, idx, &mut e, 0, &payload).unwrap();
        assert_eq!(size, 5000);
        assert!(e.start_cluster >= 2);
        let mut readback = [0u8; 5000];
        fs.read_file(&mut blk, e.start_cluster, 5000, 0, &mut readback);
        assert!(readback.iter().all(|&b| b == 0x7E));
    }
}