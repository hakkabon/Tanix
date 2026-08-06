#![allow(dead_code)]
//! Shared memory region management.
//!
//! A shared memory region is a contiguous physical range that is mapped
//! into both the primary kernel's address space and the guest VM's address
//! space.  It is the substrate for Phase 3's VirtIO ring buffers.
//!
//! Phase 2 provides:
//!   • `ShmemRegion` — descriptor for one shared region.
//!   • `alloc_shmem`  — allocate frames and record the region.
//!   • `region_for`   — look up the region for a given handle.

use crate::hypervisor::{Hypervisor, HvError, ShmemHandle};
use crate::mem::{PhysAddr, PAGE_SIZE};
use crate::mem::frame::alloc_frames;

pub struct ShmemRegion {
    pub handle: ShmemHandle,
    /// Physical base address.
    pub phys: PhysAddr,
    /// Size in bytes (multiple of PAGE_SIZE).
    pub size: usize,
}

const MAX_SHMEM: usize = 8;

pub struct ShmemTable {
    regions: [Option<ShmemRegion>; MAX_SHMEM],
}

impl ShmemTable {
    pub const fn new() -> Self {
        Self { regions: [None, None, None, None, None, None, None, None] }
    }

    /// Allocate `pages` frames, share with the hypervisor backend, and record.
    pub fn alloc(
        &mut self,
        pages: usize,
        hv: &mut dyn Hypervisor,
    ) -> Result<ShmemHandle, HvError> {
        let phys = unsafe { alloc_frames(pages) }.ok_or(HvError::NoMemory)?;
        let size = pages * PAGE_SIZE;

        // Zero the region before sharing — important for ring buffer headers.
        unsafe {
            core::ptr::write_bytes(phys as *mut u8, 0, size);
        }

        let handle = hv.mem_share(phys, size)?;

        let slot = self.regions.iter_mut().find(|s| s.is_none())
            .ok_or(HvError::NoMemory)?;
        *slot = Some(ShmemRegion { handle, phys, size });

        log::info!(
            "shmem: allocated {} pages at {:#x}, handle={:?}",
            pages, phys, handle
        );

        Ok(handle)
    }

    pub fn find(&self, handle: ShmemHandle) -> Option<&ShmemRegion> {
        self.regions
            .iter()
            .filter_map(|s| s.as_ref())
            .find(|r| r.handle == handle)
    }
}

static mut SHMEM_TABLE: ShmemTable = ShmemTable::new();

/// Allocate a shared memory region of `pages` frames.
pub unsafe fn alloc_shmem(
    pages: usize,
    hv: &mut dyn Hypervisor,
) -> Result<ShmemHandle, HvError> {
    (*core::ptr::addr_of_mut!(SHMEM_TABLE)).alloc(pages, hv)
}

/// Return the region descriptor for the given handle.
pub unsafe fn region_for(handle: ShmemHandle) -> Option<&'static ShmemRegion> {
    (*core::ptr::addr_of!(SHMEM_TABLE)).find(handle)
}

// ── Simple ring-buffer layout (foundation for Phase 3 VirtIO) ────────────────

/// A minimal shared ring buffer header placed at the start of a shmem region.
///
/// Phase 3 will replace this with a proper VirtQueue.  Phase 2 uses it for
/// the ping-pong demo: the kernel writes a slot, the guest reads and replies.
#[repr(C)]
pub struct RingHeader {
    /// Magic number — both sides check this before touching the ring.
    pub magic: u32,
    /// Number of slots in the ring.
    pub capacity: u32,
    /// Producer write index (mod capacity).
    pub write_idx: u32,
    /// Consumer read index (mod capacity).
    pub read_idx: u32,
}

impl RingHeader {
    pub const MAGIC: u32 = 0x54_41_4e_58; // "TANX"

    pub fn init(ptr: *mut Self, capacity: u32) {
        unsafe {
            core::ptr::write_volatile(ptr, RingHeader {
                magic: Self::MAGIC,
                capacity,
                write_idx: 0,
                read_idx: 0,
            });
        }
    }

    pub fn is_valid(ptr: *const Self) -> bool {
        unsafe { core::ptr::read_volatile(&(*ptr).magic) == Self::MAGIC }
    }
}
