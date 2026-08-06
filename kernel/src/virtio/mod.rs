#![allow(dead_code)]
//! VirtIO transport layer — Phase 3.
//!
//! This module implements the VirtIO 1.2 split-ring ("legacy compatible")
//! layout in shared memory.  Both the kernel (primary VM) and the Zephyr
//! stub (guest VM) map the same physical pages and operate on the rings
//! directly using volatile reads/writes and memory barriers.
//!
//! Layout within a single shared-memory region:
//!
//!   Offset 0x0000   VirtqueueConfig   (64 bytes)  — negotiated parameters
//!   Offset 0x0040   DescriptorTable   (16 × DESC_SIZE bytes)
//!   Offset 0x0240   AvailRing         (6 + 16 × 2 bytes, padded to 4 KiB)
//!   Offset 0x1000   UsedRing          (6 + 16 × 8 bytes, padded to 4 KiB)
//!   Offset 0x2000   Data buffers      (remaining pages)
//!
//! Queue depth is fixed at 16 — sufficient for Phase 3's message protocol.
//!
//! References:
//!   • VirtIO 1.2 specification §2.7 (Split Virtqueues)
//!   • <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html>

pub mod channel;
pub mod transport;

use crate::mem::PhysAddr;
use core::sync::atomic::{fence, Ordering};

// ── Queue geometry ────────────────────────────────────────────────────────────

/// Number of descriptors in each virtqueue.  Must be a power of two ≤ 32768.
pub const QUEUE_SIZE: usize = 16;

/// Size of one descriptor table entry (bytes).
pub const DESC_SIZE: usize = 16;

// ── Offsets within the shared-memory region ───────────────────────────────────

/// Offset of the VirtqueueConfig header.
pub const OFF_CONFIG:  usize = 0x0000;
/// Offset of the descriptor table.
pub const OFF_DESC:    usize = 0x0040;
/// Offset of the available ring (driver → device).
pub const OFF_AVAIL:   usize = OFF_DESC + QUEUE_SIZE * DESC_SIZE; // 0x0140
/// Offset of the used ring (device → driver), page-aligned.
pub const OFF_USED:    usize = 0x1000;
/// Offset of the data-buffer region.
pub const OFF_BUFFERS: usize = 0x2000;

/// Size of each data buffer (bytes).  Messages are at most this large.
pub const BUF_SIZE: usize = 256;

// ── Shared configuration block ────────────────────────────────────────────────

/// Magic value written by the kernel to signal the queue is ready.
pub const VIRTQ_MAGIC: u32 = 0x5649_5254; // "VIRT"

/// Placed at offset 0 in the shared-memory region.
/// Both sides check `magic == VIRTQ_MAGIC` before touching the rings.
#[repr(C)]
pub struct VirtqueueConfig {
    /// Written by kernel once rings are initialised.
    pub magic: u32,
    /// Queue depth (== QUEUE_SIZE).
    pub queue_size: u32,
    /// Physical address of the descriptor table.
    pub desc_phys: u64,
    /// Physical address of the available ring.
    pub avail_phys: u64,
    /// Physical address of the used ring.
    pub used_phys: u64,
    /// Physical base address of the data-buffer region.
    pub buf_phys: u64,
    /// Size of each buffer slot (bytes).
    pub buf_size: u32,
    /// Number of TX queue buffers owned by the kernel (primary VM).
    pub tx_slots: u32,
    _pad: [u8; 16],
}

// ── Descriptor table ──────────────────────────────────────────────────────────

/// VirtIO split-ring descriptor — §2.7.5.
#[repr(C)]
pub struct Descriptor {
    /// Guest physical address of the buffer.
    pub addr:  u64,
    /// Length of the buffer in bytes.
    pub len:   u32,
    /// Descriptor flags (VIRTQ_DESC_F_*).
    pub flags: u16,
    /// Next descriptor index (if NEXT flag set).
    pub next:  u16,
}

pub const VIRTQ_DESC_F_NEXT:     u16 = 1;
pub const VIRTQ_DESC_F_WRITE:    u16 = 2; // device-writable (used for replies)

// ── Available ring (driver → device) ─────────────────────────────────────────

/// VirtIO available ring header — §2.7.6.
#[repr(C)]
pub struct AvailRing {
    pub flags: u16,
    pub idx:   u16,
    pub ring:  [u16; QUEUE_SIZE],
    pub used_event: u16,
}

// ── Used ring (device → driver) ───────────────────────────────────────────────

/// One used-ring element.
#[repr(C)]
pub struct UsedElem {
    /// Index of the head of the consumed descriptor chain.
    pub id:  u32,
    /// Total bytes written into the buffer.
    pub len: u32,
}

/// VirtIO used ring header — §2.7.8.
#[repr(C)]
pub struct UsedRing {
    pub flags: u16,
    pub idx:   u16,
    pub ring:  [UsedElem; QUEUE_SIZE],
    pub avail_event: u16,
}

// ── Virtqueue accessor ────────────────────────────────────────────────────────

/// Live view of a single virtqueue inside a shared-memory region.
///
/// All fields are raw pointers into the shared region; access must use
/// `read_volatile` / `write_volatile` to prevent the compiler from caching
/// or reordering operations that the other VM can observe.
pub struct Virtqueue {
    pub desc:  *mut Descriptor,
    pub avail: *mut AvailRing,
    pub used:  *mut UsedRing,
    pub bufs:  *mut u8,
    /// Shadow copy of the last `used.idx` we processed (local, not shared).
    pub last_used_idx: u16,
    /// Shadow copy of the last `avail.idx` we placed (local, not shared).
    pub avail_idx: u16,
}

impl Virtqueue {
    /// Construct a Virtqueue view from the base physical address of a
    /// shared-memory region.  The region must have been initialised by
    /// `transport::init_queue`.
    ///
    /// # Safety
    /// `base_phys` must be valid, mapped, and contain an initialised
    /// VirtqueueConfig (magic == VIRTQ_MAGIC).
    pub unsafe fn from_phys(base_phys: PhysAddr) -> Self {
        let base = base_phys as *mut u8;
        Self {
            desc:  base.add(OFF_DESC)  as *mut Descriptor,
            avail: base.add(OFF_AVAIL) as *mut AvailRing,
            used:  base.add(OFF_USED)  as *mut UsedRing,
            bufs:  base.add(OFF_BUFFERS),
            last_used_idx: 0,
            avail_idx: 0,
        }
    }

    /// Pointer to buffer slot `idx`.
    pub unsafe fn buf_ptr(&self, idx: usize) -> *mut u8 {
        self.bufs.add(idx * BUF_SIZE)
    }

    // ── Kernel (primary VM) side: post a TX buffer and kick ─────────────────

    /// Write `data` into buffer slot `desc_idx`, add it to the avail ring,
    /// and advance the avail index.  The guest sees this after we doorbell.
    pub unsafe fn post_avail(&mut self, desc_idx: u16, len: u32) {
        // Fill descriptor.
        let desc = &mut *self.desc.add(desc_idx as usize);
        let buf_phys = self.bufs.add(desc_idx as usize * BUF_SIZE) as u64;
        desc.addr  = buf_phys;
        desc.len   = len;
        desc.flags = 0; // read-only from guest's perspective
        desc.next  = 0;

        // Write avail ring entry.
        let ai = self.avail_idx as usize % QUEUE_SIZE;
        let avail = &mut *self.avail;
        core::ptr::write_volatile(&mut avail.ring[ai], desc_idx);

        // Memory barrier: descriptor write must be visible before idx bump.
        fence(Ordering::Release);

        self.avail_idx = self.avail_idx.wrapping_add(1);
        core::ptr::write_volatile(&mut avail.idx, self.avail_idx);
    }

    /// Poll the used ring for completed (guest-replied) buffers.
    /// Calls `f(desc_idx, written_bytes)` for each completed entry.
    pub unsafe fn poll_used<F: FnMut(u16, u32)>(&mut self, mut f: F) {
        loop {
            let used = &*self.used;
            // Acquire barrier: used.idx write by guest must be visible.
            fence(Ordering::Acquire);
            let used_idx = core::ptr::read_volatile(&used.idx);
            if used_idx == self.last_used_idx {
                break;
            }
            let slot = self.last_used_idx as usize % QUEUE_SIZE;
            let elem = core::ptr::read_volatile(&used.ring[slot]);
            f(elem.id as u16, elem.len);
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
        }
    }

    // ── Guest (secondary VM) side: consume avail, write reply to used ────────

    /// Poll the avail ring for work posted by the kernel.
    /// Calls `f(desc_idx)` for each pending descriptor.
    pub unsafe fn guest_poll_avail<F: FnMut(u16)>(&mut self, mut f: F) {
        let avail = &*self.avail;
        fence(Ordering::Acquire);
        let avail_idx = core::ptr::read_volatile(&avail.idx);
        while self.avail_idx != avail_idx {
            let slot = self.avail_idx as usize % QUEUE_SIZE;
            let desc_idx = core::ptr::read_volatile(&avail.ring[slot]);
            f(desc_idx);
            self.avail_idx = self.avail_idx.wrapping_add(1);
        }
    }

    /// Return descriptor `desc_idx` to the kernel via the used ring.
    pub unsafe fn guest_put_used(&mut self, desc_idx: u16, written: u32) {
        let ui = self.last_used_idx as usize % QUEUE_SIZE;
        let used = &mut *self.used;
        core::ptr::write_volatile(&mut used.ring[ui], UsedElem {
            id:  desc_idx as u32,
            len: written,
        });
        fence(Ordering::Release);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        core::ptr::write_volatile(&mut used.idx, self.last_used_idx);
    }
}
