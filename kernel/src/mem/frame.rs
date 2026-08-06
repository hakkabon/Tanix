#![allow(dead_code)]
//! Physical frame allocator — bitmap over QEMU `virt` DDR.
//!
//! Memory map for QEMU `virt` (aarch64, 256 MiB):
//!   0x4000_0000 .. 0x5000_0000  — 256 MiB DDR
//!
//! The kernel image sits at 0x4008_0000.  We reserve everything below
//! `__kernel_end` (rounded up to the next page) and hand the rest out as
//! 4 KiB frames.
//!
//! The bitmap itself is stored in a static array so we need no allocator
//! to initialise the allocator.  One bit = one 4 KiB page.
//! 256 MiB / 4 KiB = 65 536 pages → 8 KiB bitmap.

use super::{PhysAddr, PAGE_SIZE};

/// Total managed DDR size (256 MiB).
const RAM_START: PhysAddr = 0x4000_0000;
const RAM_SIZE: usize = 256 * 1024 * 1024; // 256 MiB
const RAM_END: PhysAddr = RAM_START + RAM_SIZE;

const TOTAL_FRAMES: usize = RAM_SIZE / PAGE_SIZE; // 65 536
const BITMAP_WORDS: usize = TOTAL_FRAMES / 64; // 1 024 u64s = 8 KiB

/// Global frame allocator state.
///
/// A bit value of `1` means the frame is **free**; `0` means allocated.
/// Using free=1 lets us use `leading_zeros` / `trailing_zeros` to find the
/// first free frame quickly.
pub struct FrameAllocator {
    bitmap: [u64; BITMAP_WORDS],
    free_frames: usize,
}

impl FrameAllocator {
    const fn new_zeroed() -> Self {
        Self {
            bitmap: [0u64; BITMAP_WORDS],
            free_frames: 0,
        }
    }

    /// Initialise the allocator.
    ///
    /// Marks all frames free, then reserves:
    ///   1. The first 2 MiB (MMIO, firmware, stack below kernel).
    ///   2. Everything from `RAM_START` to `kernel_end` inclusive.
    ///
    /// `kernel_end` is the physical address of the first byte *after* the
    /// kernel image (read from the linker-script symbol `__kernel_end`).
    pub fn init(&mut self, kernel_end: PhysAddr) {
        // Mark everything free.
        for word in self.bitmap.iter_mut() {
            *word = !0u64;
        }
        self.free_frames = TOTAL_FRAMES;

        // Reserve frames [0 .. kernel_end] (rounded up to page boundary).
        let reserved_end = (kernel_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let reserved_frames = (reserved_end - RAM_START) / PAGE_SIZE;
        let reserved_frames = reserved_frames.min(TOTAL_FRAMES);

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

    fn phys_to_idx(addr: PhysAddr) -> Option<usize> {
        if !(RAM_START..RAM_END).contains(&addr) {
            return None;
        }
        Some((addr - RAM_START) / PAGE_SIZE)
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
            self.set_used(frame_idx);
            return Some(RAM_START + frame_idx * PAGE_SIZE);
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
        for i in 0..TOTAL_FRAMES {
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
                    return Some(RAM_START + run_start * PAGE_SIZE);
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
        let idx = Self::phys_to_idx(addr)
            .unwrap_or_else(|| panic!("free: address {:#x} out of range", addr));
        self.set_free(idx);
    }

    pub fn free_frames(&self) -> usize {
        self.free_frames
    }

    pub fn total_frames(&self) -> usize {
        TOTAL_FRAMES
    }
}

// ── Global instance ───────────────────────────────────────────────────────────

/// The single global frame allocator.
///
/// Access is intentionally not locked behind a `Mutex` — Phase 1 and 2 are
/// single-core; a spinlock will be added in Phase 4 when SMP is considered.
static mut FRAME_ALLOC: FrameAllocator = FrameAllocator::new_zeroed();

/// Initialise the global frame allocator.  Call once from `kmain`.
///
/// # Safety
/// Must be called before any call to `alloc_frame` / `free_frame`.
/// Must be called from a single thread (no concurrent callers).
pub unsafe fn init(kernel_end: PhysAddr) {
    (*core::ptr::addr_of_mut!(FRAME_ALLOC)).init(kernel_end);
    log::info!(
        "frame allocator: {} MiB free ({} frames)",
        (*core::ptr::addr_of!(FRAME_ALLOC)).free_frames() * PAGE_SIZE / (1024 * 1024),
        (*core::ptr::addr_of!(FRAME_ALLOC)).free_frames()
    );
}

/// Allocate one physical frame.  Returns `None` on OOM.
///
/// # Safety
/// `init` must have been called.
pub unsafe fn alloc_frame() -> Option<PhysAddr> {
    (*core::ptr::addr_of_mut!(FRAME_ALLOC)).alloc()
}

/// Allocate `n` contiguous physical frames.  Returns `None` on OOM.
///
/// # Safety
/// `init` must have been called.
pub unsafe fn alloc_frames(n: usize) -> Option<PhysAddr> {
    (*core::ptr::addr_of_mut!(FRAME_ALLOC)).alloc_contiguous(n)
}

/// Free a physical frame.
///
/// # Safety
/// `addr` must have been returned by `alloc_frame` / `alloc_frames` and
/// must not be freed more than once.
pub unsafe fn free_frame(addr: PhysAddr) {
    (*core::ptr::addr_of_mut!(FRAME_ALLOC)).free(addr);
}
