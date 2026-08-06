//! aarch64 exception vector table and dispatcher — Phase 3.
//!
//! Phase 3 changes from Phase 1:
//!   • `irq_handler`  — dispatches GIC interrupts (timer PPI, SGI doorbells).
//!   • `sync_handler` — dispatches synchronous exceptions from lower EL:
//!       EC 0x16 (HVC)        → hypervisor::doorbell::handle_hvc
//!       EC 0x25 (data abort) → panic with fault info
//!       other                → panic
//!   • `exception_handler` retained for unexpected/fatal exceptions.

use core::arch::global_asm;

global_asm!(include_str!("vectors.s"));

extern "C" {
    static __vectors: u8;
}

/// Install the exception vector table into VBAR_EL1.
pub fn init() {
    unsafe {
        let vbar = core::ptr::addr_of!(__vectors) as u64;
        core::arch::asm!(
            "msr VBAR_EL1, {v}",
            "isb",
            v = in(reg) vbar,
            options(nomem, nostack)
        );
    }
}

// ── ESR_EL1 exception class (EC) field — bits [31:26] ────────────────────────

const ESR_EC_SHIFT: u64 = 26;
const ESR_EC_MASK:  u64 = 0x3F;

#[inline]
fn esr_ec(esr: u64) -> u64 {
    (esr >> ESR_EC_SHIFT) & ESR_EC_MASK
}

const EC_HVC64:          u64 = 0x16; // HVC from AArch64
const EC_DABT_LOWER:     u64 = 0x24; // Data abort from lower EL
const EC_IABT_LOWER:     u64 = 0x20; // Instruction abort from lower EL

// ── IRQ handler ───────────────────────────────────────────────────────────────

/// Called from `vectors.s` slot 5 (current EL/SPx IRQ) and slot 9
/// (lower EL AArch64 IRQ).
///
/// Acknowledges the interrupt, dispatches it, then signals EOI.
#[no_mangle]
pub extern "C" fn irq_handler() {
    use super::gic;
    use crate::hypervisor::doorbell;

    let intid = gic::ack();

    match intid {
        // SGI 1 — inter-VM doorbell (guest → kernel reply notification).
        1 => {
            log::debug!("irq: SGI 1 received (VM doorbell)");
            // The actual reply is already in the used ring; the transport's
            // poll_replies() call in kmain picks it up on the next iteration.
            // Nothing more to do here.
        }

        // PPI 30 — EL1 physical timer.
        30 => {
            log::trace!("irq: timer tick");
            // Disarm to prevent continuous firing; the scheduler would
            // re-arm for the next tick (Phase 4).
            super::timer::disarm();
        }

        // SGI 0 — used for IPI / reschedule in SMP (Phase 4).
        0 => {
            log::trace!("irq: SGI 0 (reschedule)");
        }

        1023 => {
            // Spurious interrupt — GICv3 sends 1023 when no real IRQ is pending.
            // This can happen during init; just ignore it.
        }

        other => {
            log::warn!("irq: unexpected INTID={}", other);
        }
    }

    gic::eoi(intid);
    let _ = doorbell::register; // keep the import live; suppress dead-code
}

// ── Synchronous handler (lower EL) ───────────────────────────────────────────

/// Called from `vectors.s` slot 8 (lower EL AArch64 synchronous exception).
///
/// Handles HVC calls and data/instruction aborts from guest code.
/// All HVC replies are written back via x0 of the saved register frame.
///
/// The assembly stub saves registers and passes:
///   esr  — ESR_EL1
///   elr  — ELR_EL1 (faulting / HVC instruction PC)
///   far  — FAR_EL1
///   sp   — kernel stack pointer (unused here, available for future use)
#[no_mangle]
pub extern "C" fn sync_handler(esr: u64, elr: u64, far: u64, _sp: u64) {
    let ec = esr_ec(esr);

    match ec {
        EC_HVC64 => {
            // The HVC immediate encodes the call number in ESR[15:0].
            // For our SMCCC-style calls the function ID is in x0 (already
            // saved by the macro; we read it from the live register since
            // the stub hasn't zeroed it yet).
            //
            // We retrieve x0 and x1 from live registers — at this point
            // the stub has saved them on the stack but not clobbered them.
            let func: u64;
            let arg1: u64;
            unsafe {
                core::arch::asm!("mov {}, x0", out(reg) func, options(nomem, nostack));
                core::arch::asm!("mov {}, x1", out(reg) arg1, options(nomem, nostack));
            }

            // Dispatch through the doorbell handler.
            // get_backend() is a thin wrapper around the global static.
            let ret = {
                let hv = crate::hypervisor::get_backend();
                crate::hypervisor::doorbell::handle_hvc(func, arg1, hv)
            };

            // Write the return value into x0 of the saved frame.
            unsafe { core::arch::asm!("mov x0, {}", in(reg) ret, options(nomem, nostack)) };

            // Advance ELR past the HVC instruction (4 bytes).
            unsafe {
                core::arch::asm!(
                    "msr ELR_EL1, {v}",
                    v = in(reg) elr.wrapping_add(4),
                    options(nomem, nostack)
                );
            }
        }

        EC_DABT_LOWER | EC_IABT_LOWER => {
            panic!(
                "guest abort EC={:#x} ESR={:#010x} ELR={:#018x} FAR={:#018x}",
                ec, esr, elr, far
            );
        }

        other => {
            panic!(
                "unhandled sync exception EC={:#x} ESR={:#010x} ELR={:#018x}",
                other, esr, elr
            );
        }
    }
}

// ── Fatal exception handler ───────────────────────────────────────────────────

/// Called for all unexpected / fatal exception slots.
#[no_mangle]
pub extern "C" fn exception_handler(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    panic!(
        "fatal exception kind={} ESR={:#010x} ELR={:#018x} FAR={:#018x}",
        kind, esr, elr, far
    );
}
