#![allow(dead_code)]
//! Physical frame allocator — bitmap over the machine's DDR.
//!
//! The DRAM window (base + size) is discovered at boot from the device
//! tree (Phase 16); the fallback is the machine's compiled-in default
//! (`virt`: 256 MiB at 0x4000_0000, `sbsa-ref`: 1 GiB at 0x100_0000_0000).
//!
//! The kernel image sits at the DRAM base.  We reserve everything below
//! `__kernel_end` (rounded up to the next page) and hand the rest out as
//! 4 KiB frames.
//!
//! The bitmap itself is stored in a static array so we need no allocator
//! to initialise the allocator.  One bit = one 4 KiB page.  The bitmap is
//! sized for the largest supported window (1 GiB / 4 KiB = 262 144 pages
//! → 4096 u64s = 32 KiB); smaller windows simply never touch the tail.

use super::{PhysAddr, PAGE_SIZE};

/// Maximum managed DDR size the bitmap can represent (1 GiB).
const MAX_RAM_SIZE: usize = 1024 * 1024 * 1024;
const MAX_FRAMES: usize = MAX_RAM_SIZE / PAGE_SIZE; // 262 144
const BITMAP_WORDS: usize = MAX_FRAMES / 64; // 4096 u64s = 32 KiB

/// Global frame allocator state.
///
/// A bit value of `1` means the frame is **free**; `0` means allocated.
/// Using free=1 lets us use `leading_zeros` / `trailing_zeros` to find the
/// first free frame quickly.
pub struct FrameAllocator {
    /// DRAM window this allocator manages (set by `init`).
    ram_start: PhysAddr,
    ram_size: usize,
    total_frames: usize,
    bitmap: [u64; BITMAP_WORDS],
    free_frames: usize,
}

impl FrameAllocator {
    const fn new_zeroed() -> Self {
        Self {
            ram_start: 0,
            ram_size: 0,
            total_frames: 0,
            bitmap: [0u64; BITMAP_WORDS],
            free_frames: 0,
        }
    }

    /// Initialise the allocator over `[ram_start, ram_start + ram_size)`.
    ///
    /// Marks all frames free, then reserves everything from `ram_start`
    /// to `kernel_end` inclusive.
    ///
    /// `kernel_end` is the physical address of the first byte *after* the
    /// kernel image (read from the linker-script symbol `__kernel_end`).
    pub fn init(&mut self, kernel_end: PhysAddr, ram_start: PhysAddr, ram_size: usize) {
        assert!(ram_size <= MAX_RAM_SIZE, "frame: RAM too large for bitmap");
        assert_eq!(ram_start & (PAGE_SIZE - 1), 0, "frame: RAM start unaligned");

        self.ram_start = ram_start;
        self.ram_size = ram_size;
        self.total_frames = ram_size / PAGE_SIZE;

        // Mark everything free.
        for word in self.bitmap.iter_mut() {
            *word = !0u64;
        }
        self.free_frames = self.total_frames;

        // Reserve frames [0 .. kernel_end] (rounded up to page boundary).
        let reserved_end = (kernel_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let reserved_frames = (reserved_end.saturating_sub(ram_start)) / PAGE_SIZE;
        let reserved_frames = reserved_frames.min(self.total_frames);

        for i in 0..reserved_frames {
            self.set_used(i);
        }
    }

    fn set_used(&mut self, frame_idx: usize) {
        let word = frame_idx / 64;
        let bit = frame_idx % 64;
        if self.bitmap[word] & (1u64 << bit) != 0 {
            self.bitmap[word] &= !(1u64 << bit);
            self.free_frames -= 1;
        }
    }

    fn set_free(&mut self, frame_idx: usize) {
        let word = frame_idx / 64;
        let bit = frame_idx % 64;
        if self.bitmap[word] & (1u64 << bit) == 0 {
            self.bitmap[word] |= 1u64 << bit;
            self.free_frames += 1;
        }
    }

    /// Reserve a range of physical memory so it is never handed out.
    ///
    /// Rounds `start` up and `start + size` down to page boundaries; the
    /// range is clamped to the managed RAM window.
    pub fn reserve_region(&mut self, start: PhysAddr, size: usize) {
        let begin = (start + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let end = (start + size) & !(PAGE_SIZE - 1);
        if end <= begin {
            return;
        }
        let begin_idx = Self::phys_to_idx(begin, self.ram_start, self.ram_size)
            .unwrap_or(0);
        let end_idx = Self::phys_to_idx(end, self.ram_start, self.ram_size)
            .unwrap_or(self.total_frames);
        for i in begin_idx..end_idx.min(self.total_frames) {
            self.set_used(i);
        }
    }

    fn phys_to_idx(addr: PhysAddr, ram_start: PhysAddr, ram_size: usize) -> Option<usize> {
        if !(ram_start..ram_start + ram_size).contains(&addr) {
            return None;
        }
        Some((addr - ram_start) / PAGE_SIZE)
    }

    /// Allocate one physical 4 KiB frame.
    ///
    /// Returns `None` if memory is exhausted.  The returned address is
    /// page-aligned and **not** zeroed — callers must zero before use if
    /// they will be used for page tables or user data.
    pub fn alloc(&mut self) -> Option<PhysAddr> {
        for (word_idx, &word) in self.bitmap.iter().enumerate() {
            if word == 0 {
                continue;
            }
            let bit = word.trailing_zeros() as usize;
            let frame_idx = word_idx * 64 + bit;
            if frame_idx >= self.total_frames {
                return None;
            }
            self.set_used(frame_idx);
            return Some(self.ram_start + frame_idx * PAGE_SIZE);
        }
        None
    }

    /// Allocate `n` **contiguous** physical frames.
    ///
    /// Returns the base address of the first frame, or `None` if no
    /// contiguous run of `n` free frames exists.
    pub fn alloc_contiguous(&mut self, n: usize) -> Option<PhysAddr> {
        if n == 0 {
            return None;
        }
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for i in 0..self.total_frames {
            let word = i / 64;
            let bit = i % 64;
            if self.bitmap[word] & (1u64 << bit) != 0 {
                // frame is free
                if run_len == 0 {
                    run_start = i;
                }
                run_len += 1;
                if run_len == n {
                    for j in run_start..run_start + n {
                        self.set_used(j);
                    }
                    return Some(self.ram_start + run_start * PAGE_SIZE);
                }
            } else {
                run_len = 0;
            }
        }
        None
    }

    /// Free a previously allocated frame.
    ///
    /// # Panics
    /// Panics if `addr` is not page-aligned or not in the managed range.
    pub fn free(&mut self, addr: PhysAddr) {
        assert_eq!(addr & (PAGE_SIZE - 1), 0, "free: address not page-aligned");
        let idx = Self::phys_to_idx(addr, self.ram_start, self.ram_size)
            .unwrap_or_else(|| panic!("free: address {:#x} out of range", addr));
        self.set_free(idx);
    }

    pub fn free_frames(&self) -> usize {
        self.free_frames
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }
}

// ── Global instance ───────────────────────────────────────────────────────────

/// Serializes allocator access across cores (Phase 12).
///
/// Lock ordering: `FRAME_LOCK` is always taken *inside* `SCHED_LOCK`
/// (syscalls run under the scheduler lock and may allocate frames); nothing
/// ever takes `SCHED_LOCK` while holding `FRAME_LOCK`.  IRQs are masked in
/// every window where the lock can be held (the only unmasked windows — the
/// `SYS_WAIT_IRQ` wait loop and the secondary idle loop — never allocate),
/// so a holder is never preempted by a re-entrant allocation.
static FRAME_LOCK: crate::sync::SpinLock = crate::sync::SpinLock::new();

/// The single global frame allocator.
static mut FRAME_ALLOC: FrameAllocator = FrameAllocator::new_zeroed();

/// Initialise the global frame allocator over the machine's DRAM window.
/// Call once from `kmain`.
///
/// # Safety
/// Must be called before any call to `alloc_frame` / `free_frame`.
/// Must be called from a single thread (no concurrent callers).
pub unsafe fn init(kernel_end: PhysAddr, ram_start: PhysAddr, ram_size: usize) {
    (*core::ptr::addr_of_mut!(FRAME_ALLOC)).init(kernel_end, ram_start, ram_size);
    log::info!(
        "frame allocator: {} MiB free of {:#x}..{:#x} ({} frames)",
        (*core::ptr::addr_of!(FRAME_ALLOC)).free_frames() * PAGE_SIZE / (1024 * 1024),
        ram_start,
        ram_start + ram_size,
        (*core::ptr::addr_of!(FRAME_ALLOC)).free_frames()
    );
}

/// Allocate one physical frame.  Returns `None` on OOM.
///
/// # Safety
/// `init` must have been called.
pub unsafe fn alloc_frame() -> Option<PhysAddr> {
    FRAME_LOCK.lock();
    let r = (*core::ptr::addr_of_mut!(FRAME_ALLOC)).alloc();
    FRAME_LOCK.unlock();
    r
}

/// Allocate `n` contiguous physical frames.  Returns `None` on OOM.
///
/// # Safety
/// `init` must have been called.
pub unsafe fn alloc_frames(n: usize) -> Option<PhysAddr> {
    FRAME_LOCK.lock();
    let r = (*core::ptr::addr_of_mut!(FRAME_ALLOC)).alloc_contiguous(n);
    FRAME_LOCK.unlock();
    r
}

/// Reserve a range of physical memory so it is never handed out.
///
/// # Safety
/// `init` must have been called; the range must not already be in use.
pub unsafe fn reserve_region(start: PhysAddr, size: usize) {
    FRAME_LOCK.lock();
    (*core::ptr::addr_of_mut!(FRAME_ALLOC)).reserve_region(start, size);
    FRAME_LOCK.unlock();
}

/// Free a physical frame.
///
/// # Safety
/// `addr` must have been returned by `alloc_frame` / `alloc_frames` and
/// must not be freed more than once.
pub unsafe fn free_frame(addr: PhysAddr) {
    FRAME_LOCK.lock();
    (*core::ptr::addr_of_mut!(FRAME_ALLOC)).free(addr);
    FRAME_LOCK.unlock();
}

/// The managed DRAM window (after `init`).
pub fn ram_bounds() -> (PhysAddr, usize) {
    unsafe {
        (
            (*core::ptr::addr_of!(FRAME_ALLOC)).ram_start,
            (*core::ptr::addr_of!(FRAME_ALLOC)).ram_size,
        )
    }
}
