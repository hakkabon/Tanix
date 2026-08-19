//! Phase-20 filesystem server (`fs`): FAT16 over virtio-blk, served over
//! IPC.
//!
//! The server owns the virtio-blk PCI device (capability-granted ECAM +
//! BAR space, like the `net` server's NIC) and a FAT16 volume parsed by
//! `tanix-libfs`.  The protocol (namespace 0x0800) exposes open/read/
//! write/close on files in the root directory plus a listing and volume
//! info.
//!
//! On boot the server runs a self-test against the demo volume (created
//! by `scripts/mkfat16.py`): it reads README.TXT and VERSION.TXT, verifies
//! the byte pattern of DATA.BIN across its cluster chain, appends a line
//! to VISIT.LOG (exercising cluster allocation + FAT flush) and re-reads
//! it — every step is logged so a boot dump shows the whole pipeline.

#![no_std]
#![no_main]

use tanix_libdrv::blk::VirtioBlk;
use tanix_libfs::{BlockIo, DirEntry, Fat16, ShortName};
use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_FS_CLOSE, M_FS_CLOSE_REPLY, M_FS_FD_INVALID, M_FS_INFO,
    M_FS_INFO_REPLY, M_FS_LIST, M_FS_LIST_REPLY, M_FS_OPEN, M_FS_OPEN_REPLY, M_FS_READ,
    M_FS_READ_REPLY, M_FS_WRITE, M_FS_WRITE_REPLY,
};
use tanix_libsys::fmt::StrBuf;
use tanix_libsys::sys;

/// Open-file table: a name resolved to its root entry.
const N_FDS: usize = 4;

struct Fd {
    in_use: bool,
    name: ShortName,
    entry_idx: usize,
    start_cluster: u16,
    size: u32,
    writable: bool,
}

const FD_NONE: Fd = Fd {
    in_use: false,
    name: [0; 11],
    entry_idx: 0,
    start_cluster: 0,
    size: 0,
    writable: false,
};

/// The block device + mounted volume (single-threaded server: one owner).
struct FsState {
    blk: BlkDev,
    fs: Fat16,
}

static mut STATE: Option<FsState> = None;
static mut FDS: [Fd; N_FDS] = [FD_NONE; N_FDS];
static mut DEMO_BUF: [u8; 512] = [0; 512];
static mut LINE_BUF: [u8; 44] = [0; 44];

/// Local wrapper so the (foreign) `BlockIo` trait can be implemented for
/// the (foreign) virtio-blk device from this crate.
struct BlkDev(VirtioBlk);

impl BlockIo for BlkDev {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8; 512]) -> bool {
        self.0.read(sector, 1, buf)
    }
    fn write_sector(&mut self, sector: u64, buf: &[u8; 512]) -> bool {
        self.0.write(sector, 1, buf)
    }
}

/// Parse a ≤24-byte path out of a message's data words (little-endian),
/// strip leading slashes; resolves to 8.3.
fn parse_name(msg: &Message, out: &mut ShortName) -> Option<()> {
    let mut bytes = [0u8; 24];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (msg.data[i / 4] >> (8 * (i % 4))) as u8;
    }
    let n = bytes.iter().position(|&b| b == 0).unwrap_or(24);
    if !bytes[..n].is_ascii() {
        return None;
    }
    let mut start = 0;
    while start < n && bytes[start] == b'/' {
        start += 1;
    }
    if start == n {
        return None; // root itself: not a file
    }
    let s = core::str::from_utf8(&bytes[start..n]).ok()?;
    *out = Fat16::short_name(s)?;
    Some(())
}

/// Log an 8.3 short name.
fn push_name(b: &mut StrBuf, name: &ShortName) {
    let mut n = 0;
    while n < 8 && name[n] != b' ' {
        n += 1;
    }
    for i in 0..n {
        b.push_str(core::str::from_utf8(&name[i..i + 1]).unwrap_or("?"));
    }
    let mut e = 8;
    while e < 11 && name[e] != b' ' {
        e += 1;
    }
    if e > 8 {
        b.push_str(".");
        for i in 8..e {
            b.push_str(core::str::from_utf8(&name[i..i + 1]).unwrap_or("?"));
        }
    }
}

fn sched_state() -> Option<&'static mut FsState> {
    unsafe { STATE.as_mut() }
}

fn reply(dst: u32, mtype: u32, data: &[u32]) {
    let mut m = Message::new(mtype);
    for (i, v) in data.iter().take(8).enumerate() {
        m.data[i] = *v;
    }
    sys::send(dst, &m);
}

fn alloc_fd(name: ShortName, entry_idx: usize, e: &DirEntry, writable: bool) -> Option<u32> {
    unsafe {
        for i in 0..N_FDS {
            if !FDS[i].in_use {
                FDS[i] = Fd {
                    in_use: true,
                    name,
                    entry_idx,
                    start_cluster: e.start_cluster,
                    size: e.size,
                    writable,
                };
                return Some(i as u32);
            }
        }
        None
    }
}

fn fd(idx: u32) -> Option<&'static mut Fd> {
    unsafe { FDS.get_mut(idx as usize) }.filter(|f| f.in_use)
}

// ── Protocol handlers ─────────────────────────────────────────────────────────

fn serve_info(dst: u32) {
    let st = sched_state().unwrap();
    reply(
        dst,
        M_FS_INFO_REPLY,
        &[st.fs.cluster_bytes, st.fs.total_clusters as u32, st.fs.free_clusters() as u32],
    );
}

fn serve_open(dst: u32, msg: &Message) {
    let mut name = [b' '; 11];
    if parse_name(msg, &mut name).is_none() {
        reply(dst, M_FS_OPEN_REPLY, &[M_FS_FD_INVALID, 0, 0]);
        return;
    }
    let writable = msg.data[6] == 1;
    let st = sched_state().unwrap();
    let (fs, blk) = (&mut st.fs, &mut st.blk);
    match fs.find_root(&mut *blk, &name) {
        Some((idx, e)) => {
            match alloc_fd(name, idx, &e, writable) {
                Some(f) => reply(dst, M_FS_OPEN_REPLY, &[f, e.size, 1]),
                None => reply(dst, M_FS_OPEN_REPLY, &[M_FS_FD_INVALID, 0, 0]),
            }
        }
        None => {
            // Write-open creates the file if missing.
            if !writable {
                reply(dst, M_FS_OPEN_REPLY, &[M_FS_FD_INVALID, 0, 0]);
                return;
            }
            let idx = match fs.create_root_file(&mut *blk, &name) {
                Some(i) => i,
                None => {
                    reply(dst, M_FS_OPEN_REPLY, &[M_FS_FD_INVALID, 0, 0]);
                    return;
                }
            };
            let e = DirEntry::EMPTY;
            match alloc_fd(name, idx, &e, true) {
                Some(f) => reply(dst, M_FS_OPEN_REPLY, &[f, 0, 1]),
                None => reply(dst, M_FS_OPEN_REPLY, &[M_FS_FD_INVALID, 0, 0]),
            }
        }
    }
}

fn serve_read(dst: u32, msg: &Message) {
    let st = sched_state().unwrap();
    let mut data = [0u32; 8];
    match fd(msg.data[0]) {
        Some(f) => {
            let buf = unsafe { &mut *core::ptr::addr_of_mut!(DEMO_BUF) };
            let n = st
                .fs
                .read_file(&mut st.blk, f.start_cluster, f.size, msg.data[1], &mut buf[..28]);
            data[0] = n as u32;
            for (i, b) in buf[..n].iter().enumerate() {
                data[1 + i / 4] |= (*b as u32) << (8 * (i % 4));
            }
        }
        None => data[0] = M_FS_FD_INVALID,
    }
    reply(dst, M_FS_READ_REPLY, &data);
}

fn serve_write(dst: u32, msg: &Message) {
    let mut data = [0u32; 8];
    let n = (msg.data[2] as usize).min(20);
    let off = msg.data[1];
    match fd(msg.data[0]) {
        Some(f) => {
            if !f.writable {
                data[0] = 0;
                data[1] = f.size;
                reply(dst, M_FS_WRITE_REPLY, &data);
                return;
            }
            if off > f.size {
                data[0] = 0; // offset must be ≤ size (overwrite) or == size (append)
                data[1] = f.size;
                reply(dst, M_FS_WRITE_REPLY, &data);
                return;
            }
            let mut payload = [0u8; 20];
            for i in 0..n {
                payload[i] = (msg.data[3 + i / 4] >> (8 * (i % 4))) as u8;
            }
            let st = sched_state().unwrap();
            let (fs, blk) = (&mut st.fs, &mut st.blk);
            let mut e = match fs.find_root(&mut *blk, &f.name) {
                Some((_, e)) => e,
                None => {
                    data[0] = 0;
                    reply(dst, M_FS_WRITE_REPLY, &data);
                    return;
                }
            };
            match fs.write_file(&mut *blk, f.entry_idx, &mut e, off, &payload[..n]) {
                Some(sz) => {
                    // Refresh the fd from disk (write_file may have
                    // allocated the first cluster).
                    match fs.find_root(&mut *blk, &f.name) {
                        Some((_, fresh)) => {
                            f.start_cluster = fresh.start_cluster;
                            f.size = fresh.size;
                        }
                        None => {}
                    }
                    data[0] = 1;
                    data[1] = sz;
                }
                None => data[0] = 0,
            }
        }
        None => data[0] = 0,
    }
    reply(dst, M_FS_WRITE_REPLY, &data);
}

fn serve_close(dst: u32, msg: &Message) {
    let mut ok = 0u32;
    match fd(msg.data[0]) {
        Some(f) => {
            f.in_use = false;
            ok = 1;
        }
        None => {}
    }
    reply(dst, M_FS_CLOSE_REPLY, &[ok]);
}

/// The `offset`-th live root entry (skipping deleted/volume entries).
/// Returns (name word0, word1, is_dir, size), or None past the end.
fn next_root_entry(offset: usize) -> Option<(u32, u32, u32, u32)> {
    let st = sched_state().unwrap();
    let mut seen = 0usize;
    for idx in 0..st.fs.root_entries as usize {
        let mut sec = [0u8; 512];
        if !st.blk.read_sector(st.fs.root_base + (idx / 16) as u64, &mut sec) {
            return None;
        }
        let e: DirEntry = unsafe { core::ptr::read_unaligned(sec.as_ptr().add((idx % 16) * 32).cast()) };
        if e.is_empty() {
            return None; // end of directory
        }
        if e.is_deleted() || e.is_volume() {
            continue;
        }
        if seen < offset {
            seen += 1;
            continue;
        }
        let (w0, w1) = e.name_words();
        return Some((w0, w1, e.is_dir() as u32, e.size));
    }
    None
}

fn serve_list(dst: u32, msg: &Message) {
    let mut data = [0u32; 8];
    match next_root_entry(msg.data[0] as usize) {
        Some((w0, w1, is_dir, size)) => {
            data[0] = 1;
            data[1] = w0;
            data[2] = w1;
            data[3] = is_dir;
            data[4] = size;
        }
        None => data[0] = 0,
    }
    reply(dst, M_FS_LIST_REPLY, &data);
}

// ── Boot self-test ────────────────────────────────────────────────────────────

fn log_size(name: &str, n: usize) {
    let mut b = StrBuf::new();
    b.push_str(name);
    b.push_str(" size ");
    b.push_dec32(n as u32);
    sys::log(0, b.as_str());
}

fn demo_read_file(name: &ShortName, out: &mut [u8]) -> usize {
    let st = sched_state().unwrap();
    let mut total = 0usize;
    if let Some((_, e)) = st.fs.find_root(&mut st.blk, name) {
        let mut off = 0u32;
        loop {
            let n = st.fs.read_file(&mut st.blk, e.start_cluster, e.size, off, out);
            if n == 0 || total + n > out.len() {
                break;
            }
            total += n;
            off += n as u32;
            if off >= e.size {
                break;
            }
        }
    }
    total
}

fn demo() {
    // Volume info.
    {
        let st = sched_state().unwrap();
        let mut b = StrBuf::new();
        b.push_str("fs: FAT16 mounted cluster ");
        b.push_dec32(st.fs.cluster_bytes as u32);
        b.push_str(" total ");
        b.push_dec32(st.fs.total_clusters as u32);
        b.push_str(" free ");
        b.push_dec32(st.fs.free_clusters() as u32);
        sys::log(0, b.as_str());
    }

    // Root listing.
    for i in 0..8 {
        match next_root_entry(i) {
            Some((w0, w1, _, size)) => {
                let mut n = ShortName::default();
                for (j, w) in [w0, w1].iter().enumerate() {
                    for k in 0..4 {
                        n[j * 4 + k] = ((*w >> (8 * k)) & 0xFF) as u8;
                    }
                }
                let mut b = StrBuf::new();
                b.push_str("fs: root ");
                push_name(&mut b, &n);
                b.push_str(" size ");
                b.push_dec32(size);
                sys::log(0, b.as_str());
            }
            None => break,
        }
    }

    // README.TXT + VERSION.TXT.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(DEMO_BUF) };
    {
        let name = Fat16::short_name("README.TXT").unwrap();
        let n = demo_read_file(&name, buf);
        log_size("fs: demo README.TXT", n);
        sys::log(0, "fs: demo README.TXT contents:");
        // Log the first two lines.
        let mut start = 0usize;
        let mut line = 0u32;
        for i in 0..n {
            if buf[i] == b'\n' {
                let mut b = StrBuf::new();
                b.push_str("    | ");
                for &c in &buf[start..i] {
                    b.push_str(core::str::from_utf8(&[c]).unwrap_or("?"));
                }
                sys::log(0, b.as_str());
                start = i + 1;
                line += 1;
                if line >= 3 {
                    break;
                }
            }
        }
    }
    {
        let name = Fat16::short_name("VERSION.TXT").unwrap();
        let n = demo_read_file(&name, buf);
        let s = core::str::from_utf8(&buf[..n.min(64)]).unwrap_or("?");
        let mut b = StrBuf::new();
        b.push_str("fs: demo VERSION.TXT: ");
        b.push_str(s);
        sys::log(0, b.as_str());
    }

    // DATA.BIN: 3000-byte pattern, byte i = i % 251.
    {
        let name = Fat16::short_name("DATA.BIN").unwrap();
        let st = sched_state().unwrap();
        let e = st.fs.find_root(&mut st.blk, &name).unwrap().1;
        let mut ok = true;
        let mut off = 0u32;
        let mut chunks = 0u32;
        while off < e.size {
            let want = (e.size - off).min(256) as usize;
            let n = st.fs.read_file(&mut st.blk, e.start_cluster, e.size, off, &mut buf[..want]);
            for (j, b) in buf[..n].iter().enumerate() {
                if *b != ((off as usize + j) % 251) as u8 {
                    ok = false;
                    break;
                }
            }
            chunks += 1;
            off += n as u32;
            if n == 0 {
                break;
            }
        }
        let mut b = StrBuf::new();
        b.push_str("fs: demo DATA.BIN ");
        if ok && off == e.size {
            b.push_str("verified (");
            b.push_dec32(e.size);
            b.push_str(" bytes, ");
            b.push_dec32(chunks);
            b.push_str(" chunks)");
        } else {
            b.push_str("MISMATCH");
        }
        sys::log(0, b.as_str());
    }

    // VISIT.LOG: append a line (cluster allocation + FAT flush + re-read).
    {
        let name = Fat16::short_name("VISIT.LOG").unwrap();
        let st = sched_state().unwrap();
        let before = st.fs.free_clusters();
        let (idx, mut e) = match st.fs.find_root(&mut st.blk, &name) {
            Some(x) => x,
            None => {
                let idx = st.fs.create_root_file(&mut st.blk, &name).unwrap();
                (idx, DirEntry::EMPTY)
            }
        };
        let line = unsafe { &mut *core::ptr::addr_of_mut!(LINE_BUF) };
        let text = b"tanix fs demo visit\n";
        line[..text.len()].copy_from_slice(text);
        let eof = e.size;
        let new_size = st
            .fs
            .write_file(&mut st.blk, idx, &mut e, eof, &line[..text.len()])
            .unwrap();
        let after = st.fs.free_clusters();
        let mut b = StrBuf::new();
        b.push_str("fs: demo VISIT.LOG appended -> size ");
        b.push_dec32(new_size);
        b.push_str(" (free clusters ");
        b.push_dec32(before as u32);
        b.push_str(" -> ");
        b.push_dec32(after as u32);
        b.push_str(")");
        sys::log(0, b.as_str());
        // Re-read the last line to prove persistence.
        let st = sched_state().unwrap();
        let e = st.fs.find_root(&mut st.blk, &name).unwrap().1;
        let n = st.fs.read_file(&mut st.blk, e.start_cluster, e.size, e.size - text.len() as u32, buf);
        let s = core::str::from_utf8(&buf[..n]).unwrap_or("?");
        let mut b = StrBuf::new();
        b.push_str("fs: demo VISIT.LOG re-read tail: ");
        b.push_str(s);
        sys::log(0, b.as_str());
    }

    sys::log(0, "fs: self-test complete");
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "fs: up");

    let blk = match VirtioBlk::open() {
        Some(b) => b,
        None => {
            sys::log(1, "fs: no virtio-blk device — idling");
            loop {
                let _ = sys::receive(M_ANY);
            }
        }
    };
    let mut blk = BlkDev(blk);
    let fs = Fat16::mount(&mut blk).unwrap_or_else(|| {
        sys::log(1, "fs: no FAT16 boot sector — idling");
        loop {
            let _ = sys::receive(M_ANY);
        }
    });
    unsafe {
        STATE = Some(FsState { blk, fs });
    }

    // Self-test first (before the event loop can interleave requests).
    demo();
    sys::log(0, "fs: serving");

    loop {
        let (src, msg) = sys::receive(M_ANY);
        match msg.mtype {
            M_FS_INFO => serve_info(src),
            M_FS_OPEN => serve_open(src, &msg),
            M_FS_READ => serve_read(src, &msg),
            M_FS_WRITE => serve_write(src, &msg),
            M_FS_CLOSE => serve_close(src, &msg),
            M_FS_LIST => serve_list(src, &msg),
            _ => {}
        }
    }
}