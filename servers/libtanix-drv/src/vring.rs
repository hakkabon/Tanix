//! A modern (virtio 1.0) **split vring** — Phase 10.
//!
//! The legacy mmio transport (Phase 5, `servers/display`) addressed a
//! single contiguous ring via a physical frame number; the modern
//! transport instead takes three independent 64-bit addresses (descriptor
//! table, avail ring, used ring).  We give each its own 4 KiB frame, which
//! trivially satisfies the alignment requirements.
//!
//! Layout per queue:
//!   • frame 0 — descriptor table: `QUEUE_SIZE × 16` B entries
//!   • frame 1 — avail ring: flags, idx, `QUEUE_SIZE × 2` B heads
//!   • frame 2 — used ring: flags, idx, `QUEUE_SIZE × 8` B entries
//!
//! Ring size is a power of two (`QUEUE_SIZE`).  The *driver* (net.rs)
//! owns slot assignment: the RX pool lives in fixed low slots (re-published
//! after every drain) and TX chains use fixed high slots — the net server
//! has at most one outstanding transmit, so no free-list is needed.
//! All ring updates follow the spec ordering: descriptors written first,
//! then a Release fence, then `avail_idx` published; used entries are read
//! with an Acquire fence.
//!
//! The rings live in frames allocated with `SYS_ALLOC_FRAMES` (identity
//! mapped, directly dereferenceable).

use core::ptr;

use tanix_libsys::sys;

/// Number of descriptor slots per vring (power of two).
pub const QUEUE_SIZE: u16 = 64;

/// Descriptor flags.
pub const DESC_NEXT: u16 = 1; // buffer continues at `next`
pub const DESC_WRITE: u16 = 2; // device writes into this buffer

/// A 16-byte split-ring descriptor.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// Used-ring element: descriptor id + bytes written by the device.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UsedElem {
    pub id: u32,
    pub len: u32,
}

/// A vring spanning three frames.
pub struct Vring {
    /// Physical (== virtual, identity map) addresses of the three rings.
    pub desc_base: u64,
    pub avail_base: u64,
    pub used_base: u64,
    /// How far the driver has consumed the used ring.
    used_tail: u16,
}

impl Vring {
    /// Allocate the three backing frames and zero them.
    pub fn new() -> Option<Vring> {
        let base = sys::alloc_frames(3);
        if base == 0 {
            return None;
        }
        unsafe {
            ptr::write_bytes(base as *mut u8, 0, 3 * 4096);
        }
        Some(Vring {
            desc_base: base,
            avail_base: base + 0x1000,
            used_base: base + 0x2000,
            used_tail: 0,
        })
    }

    #[inline]
    fn descs(&self) -> *mut Desc {
        self.desc_base as *mut Desc
    }

    #[inline]
    fn avail_idx_ptr(&self) -> *mut u16 {
        (self.avail_base + 2) as *mut u16
    }

    #[inline]
    fn avail_ring(&self) -> *mut u16 {
        (self.avail_base + 4) as *mut u16
    }

    #[inline]
    fn used_idx_ptr(&self) -> *mut u16 {
        (self.used_base + 2) as *mut u16
    }

    #[inline]
    fn used_ring(&self) -> *mut UsedElem {
        (self.used_base + 4) as *mut UsedElem
    }

    /// Write a descriptor entry.  `slot` must be < QUEUE_SIZE.
    pub fn write_desc(&mut self, slot: u16, addr: u64, len: u32, flags: u16, next: u16) {
        unsafe {
            ptr::write_volatile(self.descs().add(slot as usize), Desc { addr, len, flags, next });
        }
    }

    /// Publish a chain head in the avail ring (Release order).
    pub fn publish(&mut self, head: u16) {
        let idx = unsafe { ptr::read_volatile(self.avail_idx_ptr()) };
        unsafe {
            ptr::write_volatile(self.avail_ring().add((idx % QUEUE_SIZE) as usize), head);
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        unsafe {
            ptr::write_volatile(self.avail_idx_ptr(), idx.wrapping_add(1));
        }
    }

    /// Read the device's current used index (Acquire order).
    pub fn used_idx(&self) -> u16 {
        let idx = unsafe { ptr::read_volatile(self.used_idx_ptr()) };
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        idx
    }

    /// True if the device has returned at least one entry.
    pub fn has_used(&self) -> bool {
        (self.used_idx() as usize).wrapping_sub(self.used_tail as usize) > 0
    }

    /// Pop the oldest returned entry `(descriptor id, len)`.
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        let used = self.used_idx();
        if (used as usize).wrapping_sub(self.used_tail as usize) == 0 {
            return None;
        }
        let e = unsafe { ptr::read_volatile(self.used_ring().add((self.used_tail % QUEUE_SIZE) as usize)) };
        self.used_tail = self.used_tail.wrapping_add(1);
        Some((e.id as u16, e.len))
    }

    /// Drain every returned entry into `out`; returns the count.
    pub fn drain_used(&mut self, out: &mut [(u16, u32)]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.pop_used() {
                Some(e) => {
                    out[n] = e;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }
}
