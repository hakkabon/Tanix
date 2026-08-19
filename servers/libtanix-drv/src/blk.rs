//! virtio-blk driver — Phase 20.
//!
//! A synchronous block device on the same modern (virtio 1.0) virtio-pci
//! transport as the NIC: one request queue, one request in flight at a
//! time.  Each request is a three-descriptor chain:
//!
//!   • desc 0 — the fixed 16-byte `virtio_blk_req` header
//!               (type u32, reserved u32, sector u64);
//!   • desc 1 — the data buffer (`DESC_WRITE` for reads, plain for
//!               writes); up to 8 sectors (4 KiB) per request;
//!   • desc 2 — the 1-byte status slot (`DESC_WRITE`; 0 = VIRTIO_BLK_S_OK).
//!
//! Completion is polled: after the notify kick the driver spins on the
//! used ring (with `SYS_CACHE_SYNC` barriers — Phase 16) and bounds the
//! wait, so the calls are safe to make from a server's event loop even
//! though the device normally signals via its INTx line.

use core::ptr;

use tanix_libsys::sys;

use crate::virtio_pci::VirtioPci;
use crate::vring::{Vring, DESC_WRITE};

/// virtio-blk device id.
pub const VIRTIO_BLK_S: u16 = 2;

/// Mandated by v1.0; negotiated for cleanliness like the NIC driver.
const F_VERSION_1: u64 = 1 << 32;

/// Virtio-blk request types.
const REQ_TYPE_IN: u32 = 0; // device → driver (read)
const REQ_TYPE_OUT: u32 = 1; // driver → device (write)

/// 512-byte sectors, virtio-blk's fixed geometry (QEMU default).
pub const SECTOR_SIZE: usize = 512;
/// Maximum sectors per request (data buffer is one 4 KiB frame).
pub const MAX_SECTORS: u32 = 8;
/// Data buffer size per request (frames).
const DATA_BYTES: usize = SECTOR_SIZE * MAX_SECTORS as usize;

/// Virtio-blk request header (16 bytes, little-endian fields).
#[repr(C)]
struct BlkReq {
    typ: u32,
    _reserved: u32,
    sector: u64,
}

/// Queue + descriptor slots (fixed: one request in flight).
const QUEUE: u16 = 0;
const SLOT_HEADER: u16 = 0;
const SLOT_DATA: u16 = 1;
const SLOT_STATUS: u16 = 2;

/// Status byte values.
const S_OK: u8 = 0;

/// A bound on the completion spin: ~2 M poll rounds.  QEMU completes a
/// 4 KiB request in microseconds; anything beyond this is a real fault.
const WAIT_ROUNDS: u32 = 2_000_000;

/// An open virtio-blk device.
pub struct VirtioBlk {
    pub dev: VirtioPci,
    q: Vring,
    /// Frame for the 16-byte request header + status byte.
    pub header_base: u64,
    /// Frame for the request's data buffer (4 KiB).
    pub data_base: u64,
    /// Capacity in sectors (device config, for logging).
    pub capacity_sectors: u64,
}

impl VirtioBlk {
    /// Probe and bring up the virtio-blk device.
    pub fn open() -> Option<VirtioBlk> {
        let dev = VirtioPci::open(VIRTIO_BLK_S)?;

        dev.reset();
        let feats = dev.negotiate(F_VERSION_1)?;
        if feats & F_VERSION_1 == 0 {
            dev.fail("virtio-blk: device has no VERSION_1 feature");
            return None;
        }

        let q = Vring::new()?;
        if dev.setup_queue(QUEUE, &q) == 0 {
            return None;
        }

        let header_base = sys::alloc_frames(1);
        let data_base = sys::alloc_frames(DATA_BYTES.div_ceil(4096) as u32);
        if header_base == 0 || data_base == 0 {
            sys::log(1, "virtio-blk: buffer alloc failed");
            return None;
        }
        unsafe {
            ptr::write_bytes(header_base as *mut u8, 0, 4096);
        }

        // Device config: the 64-bit sector count at offset 0.
        let capacity_sectors = dev.cfg_read8(0) as u64
            | ((dev.cfg_read8(1) as u64) << 8)
            | ((dev.cfg_read8(2) as u64) << 16)
            | ((dev.cfg_read8(3) as u64) << 24)
            | ((dev.cfg_read8(4) as u64) << 32)
            | ((dev.cfg_read8(5) as u64) << 40)
            | ((dev.cfg_read8(6) as u64) << 48)
            | ((dev.cfg_read8(7) as u64) << 56);

        dev.driver_ok();

        let mut b = tanix_libsys::fmt::StrBuf::new();
        b.push_str("virtio-blk: up, capacity ");
        b.push_dec32((capacity_sectors / 2048) as u32); // MiB (512-byte sectors)
        b.push_str(" MiB");
        sys::log(0, b.as_str());

        Some(VirtioBlk { dev, q, header_base, data_base, capacity_sectors })
    }

    /// Issue `read` / `write` of `n` sectors (1..=8) at `sector` and wait
    /// for completion.  The data buffer must be `[data_base, data_base+4KiB)`
    /// — it is what the device DMA-touches.
    fn rw(&mut self, write: bool, sector: u64, n: u32, len: usize) -> bool {
        let _ = n;

        // Zero the header, then fill the request.
        let header = self.header_base as *mut BlkReq;
        unsafe {
            ptr::write_bytes(header as *mut u8, 0, core::mem::size_of::<BlkReq>() as usize);
            (*header).typ = if write { REQ_TYPE_OUT } else { REQ_TYPE_IN };
            (*header).sector = sector;
        }
        // Status slot behind the header.
        unsafe {
            ptr::write_volatile((self.header_base + 512) as *mut u8, 0xFF);
        }

        // Chain: header → data → status.
        self.q.write_desc(
            SLOT_HEADER,
            self.header_base,
            core::mem::size_of::<BlkReq>() as u32,
            0,
            SLOT_DATA,
        );
        self.q.write_desc(
            SLOT_DATA,
            self.data_base,
            len as u32, // bytes the device transfers
            if write { 0 } else { DESC_WRITE },
            SLOT_STATUS,
        );
        self.q.write_desc(SLOT_STATUS, self.header_base + 512, 1, DESC_WRITE, 0);
        self.q.publish(SLOT_HEADER);

        // Phase 16: clean the descriptors + buffers before the device reads
        // them, and after the device writes them (poll below).
        sys::cache_sync();
        self.dev.notify(QUEUE);

        let mut rounds = 0u32;
        loop {
            if self.dev.read_isr() & 1 != 0 {
                // Deassert the INTx line; the used ring carries the result.
            }
            sys::cache_sync();
            match self.q.pop_used() {
                Some((id, _)) => {
                    if id != SLOT_HEADER {
                        return false;
                    }
                    let st = unsafe { ptr::read_volatile((self.header_base + 512) as *const u8) };
                    return st == S_OK;
                }
                None => {}
            }
            rounds += 1;
            if rounds >= WAIT_ROUNDS {
                sys::log(1, "virtio-blk: request timed out");
                return false;
            }
        }
    }

    /// Read sectors `sector..sector+n` into `buf` (must hold `n*512` B).
    pub fn read(&mut self, sector: u64, n: u32, buf: &mut [u8]) -> bool {
        if n == 0 || n > MAX_SECTORS || buf.len() < n as usize * SECTOR_SIZE {
            return false;
        }
        if !self.rw(false, sector, n, n as usize * SECTOR_SIZE) {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(self.data_base as *const u8, buf.as_mut_ptr(), n as usize * SECTOR_SIZE);
        }
        true
    }

    /// Write sectors `sector..sector+n` from `buf`.
    pub fn write(&mut self, sector: u64, n: u32, buf: &[u8]) -> bool {
        if n == 0 || n > MAX_SECTORS || buf.len() < n as usize * SECTOR_SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(buf.as_ptr(), self.data_base as *mut u8, n as usize * SECTOR_SIZE);
        }
        self.rw(true, sector, n, n as usize * SECTOR_SIZE)
    }
}