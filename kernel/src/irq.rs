//! Interrupt pending bookkeeping — Phase 7.
//!
//! The GIC is the single interrupt source on this CPU (PPIs 16..31, SPIs
//! 32..1019).  `irq_handler` records every delivered device interrupt in a
//! pending bitmap; the `SYS_WAIT_IRQ` syscall parks the calling task until
//! its IRQ's bit is set (polling the bit with interrupts unmasked).
//!
//! The bitmap covers PPIs + the virtio-mmio SPI window (SPIs 48..79); the
//! wait loop treats any recorded IRQ as "serviced and pending".
//!
//! Single-CPU, so no atomics are required: the handler runs on the current
//! task's kernel stack, and the only concurrent observer (the wait loop)
//! polls the bit between interrupt windows.

/// PPI 16..31 + virtio SPIs 48..79 — anything the syscall may wait on.
pub const IRQ_MAX: usize = 96;

static mut PENDING: [bool; IRQ_MAX] = [false; IRQ_MAX];

/// Record a delivered interrupt (called from `irq_handler`).
pub fn note_pending(intid: u32) {
    if (intid as usize) < IRQ_MAX {
        unsafe {
            core::ptr::write_volatile(&raw mut PENDING[intid as usize], true);
        }
    }
}

/// True if `intid` has been delivered and not yet consumed.
///
/// Volatile read: the SYS_WAIT_IRQ wait loop spins on this bit while the
/// IRQ handler (which runs between `wfi` wake-ups, on the same core) sets
/// it.  The `wfi` instruction is `nomem` to the compiler, so a plain load
/// could legally be hoisted out of the loop — the volatile read keeps the
/// loop observing the handler's stores.
pub fn pending(intid: u32) -> bool {
    if (intid as usize) < IRQ_MAX {
        unsafe { core::ptr::read_volatile(&raw const PENDING[intid as usize]) }
    } else {
        false
    }
}

/// Test-and-clear: consume a recorded interrupt.
pub fn take_pending(intid: u32) -> bool {
    if pending(intid) {
        clear_pending(intid);
        true
    } else {
        false
    }
}

/// Clear a recorded interrupt.
pub fn clear_pending(intid: u32) {
    if (intid as usize) < IRQ_MAX {
        unsafe {
            core::ptr::write_volatile(&raw mut PENDING[intid as usize], false);
        }
    }
}
