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
//! Phase 12 (SMP): the bits are atomics — device SPIs are routed to CPU 0
//! only, but a `SYS_WAIT_IRQ` waiter may be parked in `wfi` on any core,
//! and the `irq_handler` on CPU 0 both records the bit here and SGIs the
//! other cores so their wait loops re-poll it.  Cross-core writers and
//! readers need the atomicity + ordering (AcqRel between the handler's
//! record and a woken core's poll).

use core::sync::atomic::{AtomicBool, Ordering};

/// PPI 16..31 + virtio SPIs 48..79 — anything the syscall may wait on.
pub const IRQ_MAX: usize = 96;

static PENDING: [AtomicBool; IRQ_MAX] = [const { AtomicBool::new(false) }; IRQ_MAX];

/// Record a delivered interrupt (called from `irq_handler`).
pub fn note_pending(intid: u32) {
    if (intid as usize) < IRQ_MAX {
        PENDING[intid as usize].store(true, Ordering::Release);
    }
}

/// True if `intid` has been delivered and not yet consumed.
///
/// Acquire load: the `SYS_WAIT_IRQ` wait loop spins on this bit while the
/// IRQ handler (on CPU 0 — possibly a *different* core, Phase 12) sets it.
/// The `wfi` instruction is `nomem` to the compiler, so a plain load could
/// legally be hoisted out of the loop — the atomic read keeps the loop
/// observing the handler's stores.
pub fn pending(intid: u32) -> bool {
    if (intid as usize) < IRQ_MAX {
        PENDING[intid as usize].load(Ordering::Acquire)
    } else {
        false
    }
}

/// Test-and-clear: consume a recorded interrupt.
pub fn take_pending(intid: u32) -> bool {
    if (intid as usize) < IRQ_MAX {
        PENDING[intid as usize].swap(false, Ordering::AcqRel)
    } else {
        false
    }
}

/// Clear a recorded interrupt.
pub fn clear_pending(intid: u32) {
    if (intid as usize) < IRQ_MAX {
        PENDING[intid as usize].store(false, Ordering::Release);
    }
}
