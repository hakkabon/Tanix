//! Minimal IRQ-safe spinlock — Phase 11 (SMP).
//!
//! The kernel schedules with IRQs masked (exception entry masks DAIF),
//! so a plain spinlock suffices: no IRQ can fire on the spinning CPU
//! while it waits, and the holder can never be preempted mid-critical-
//! section by an interrupt on the same CPU.

#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, Ordering};

/// Simple acquire/release spinlock.
///
/// `#[repr(C)]` pins the layout for `context_switch_unlock` (switch.s):
/// `held` is a single byte at offset 0, released with `stlrb`.
#[repr(C)]
pub struct SpinLock {
    held: AtomicBool,
}

impl SpinLock {
    pub const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    /// Acquire the lock, spinning with `pause`-style backoff.
    pub fn lock(&self) {
        while self.held.swap(true, Ordering::Acquire) {
            while self.held.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Release the lock.
    pub fn unlock(&self) {
        self.held.store(false, Ordering::Release);
    }

    /// True if any CPU currently holds the lock (debugging aid).
    pub fn is_locked(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }
}
