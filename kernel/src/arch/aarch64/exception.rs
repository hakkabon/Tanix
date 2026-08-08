//! aarch64 exception vector table and dispatcher — Phase 6.
//!
//! Phase 6 changes from Phase 3:
//!   • `sync_handler` — dispatches synchronous exceptions from lower EL:
//!       EC 0x15 (SVC64)     → handled in vectors.s (tanix_syscall fast path)
//!       EC 0x16 (HVC)       → hypervisor::doorbell::handle_hvc
//!       EC 0x20/0x24 (aborts) → EL0 tasks are killed (isolation proof);
//!                                EL1 guest aborts panic as before
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
const EC_IABT_LOWER:     u64 = 0x20; // Instruction abort from lower EL
const EC_DABT_LOWER:     u64 = 0x24; // Data abort from lower EL

// ── IRQ handler ───────────────────────────────────────────────────────────────

/// Called from `vectors.s` slot 5 (current EL/SPx IRQ — `from_el0 == 0`)
/// and slot 9 (lower EL AArch64 IRQ — `from_el0 == 1`).
///
/// Acknowledges the interrupt, dispatches it, signals EOI, then lets the
/// scheduler preempt when the tick interrupted an EL0 task.
#[no_mangle]
pub extern "C" fn irq_handler(from_el0: u64) {
    use super::gic;
    use crate::sched::task;

    let intid = gic::ack();

    match intid {
        // PPI 30 — EL1 physical timer (CNTPNSIRQ on QEMU `virt`; the board
        // maps the EL1 timer to INTID 30, the EL2 timer to 26).
        // Phase 7: count + re-arm the tick, then let the scheduler
        // preempt (only when the tick hit an EL0 task — ticks inside the
        // kernel, e.g. the SYS_WAIT_IRQ wait loop, just tick).
        30 => {
            super::timer::tick();
            log::trace!("irq: tick #{}", super::timer::ticks());
            gic::eoi(intid);
            unsafe {
                task::tick_preempt(from_el0 != 0);
            }
        }

        // SGI 1 — inter-VM doorbell (guest → kernel reply notification).
        1 => {
            log::debug!("irq: SGI 1 received (VM doorbell)");
            gic::eoi(intid);
        }

        // SGI 0 — used for IPI / reschedule in SMP (Phase 4).
        0 => {
            log::trace!("irq: SGI 0 (reschedule)");
            gic::eoi(intid);
        }

        // Any other wired interrupt (device IRQs: virtio-mmio SPIs
        // 48..79): record it for SYS_WAIT_IRQ and signal EOI.  The waiter
        // may be inside its wait loop (current-EL IRQ) or about to call
        // wait_irq from EL0 — in both cases the pending bit is how it
        // learns the device completed.
        other if (16..crate::irq::IRQ_MAX as u32).contains(&other) => {
            log::trace!("irq: device INTID={} (pending)", other);
            crate::irq::note_pending(other);
            // Level-triggered device IRQ: mask it *before* EOI.  The level
            // stays asserted until the driver's INT_ACK, so an unmasked
            // EOI would make the GIC re-fire in the instruction window
            // before the SYS_WAIT_IRQ wait loop resumes — starving the
            // loop (it can never observe the pending bit).  The waiter
            // consumes the bit and the next wait_irq re-enables the IRQ.
            gic::disable_irq(other);
            gic::eoi(intid);
        }

        1023 => {
            // Spurious interrupt — GICv3 sends 1023 when no real IRQ is
            // pending.  This can happen during init; just ignore it.
            gic::eoi(intid);
        }

        other => {
            log::warn!("irq: unexpected INTID={}", other);
            gic::eoi(intid);
        }
    }
}

// ── Synchronous handler (lower EL) ───────────────────────────────────────────

/// SPSR_EL1 saved in the exception frame — the exception level of the
/// interrupted context is PSTATE.M[3:0] (0 = EL0t, 4 = EL1h, 5 = EL1t).
fn from_el0(frame: *const u64) -> bool {
    unsafe { core::ptr::read_volatile(frame.add(21)) & 0xF == 0 }
}

/// Abort from an EL0 task: log, mark it zombie and switch away.  Never
/// returns.  The kernel itself (and the EL1 guest) are unaffected — this
/// is the Phase-6 isolation guarantee.
unsafe fn kill_faulting_task(esr: u64, elr: u64, far: u64) -> ! {
    use crate::sched::task::{context_switch, scheduler};
    use crate::sched::TaskState;

    let sched = scheduler();
    let idx = sched.current_idx();
    let name = sched.current_name();
    let id = sched.current_id();
    log::error!(
        "EL0 fault: task {:?} '{}' ESR={:#010x} ELR={:#018x} FAR={:#018x} — killed",
        id, name, esr, elr, far
    );
    sched.set_state(idx, TaskState::Zombie);
    if let Some(t) = sched.task_at_mut(idx) {
        t.recv_blocked = false;
        t.recv_buf = core::ptr::null_mut();
    }

    // Switch to the next runnable task; the zombie never runs again.
    loop {
        let next = sched.pick_next();
        if next == idx {
            // Nothing else runnable — fall back to the boot context.
            let from = sched.ctx_ptr(idx);
            let to = sched.ctx_ptr(0);
            sched.set_current(0);
            context_switch(from, to);
        } else {
            let from = sched.ctx_ptr(idx);
            let to = sched.ctx_ptr(next);
            sched.set_state(next, TaskState::Running);
            sched.set_current(next);
            context_switch(from, to);
        }
    }
}

/// Called from `vectors.s` slot 8 (lower EL AArch64 synchronous exception).
///
/// Handles HVC calls and data/instruction aborts from guest code.
///
/// The assembly stub saves the guest's register file on the *guest's* stack
/// and passes us the frame base:
///   esr  — ESR_EL1
///   elr  — ELR_EL1 (faulting / HVC instruction PC)
///   far  — FAR_EL1
///   sp   — saved-frame base pointer
///
/// Frame layout (set up by `LOWER_SYNC_ENTRY` in vectors.s):
///   [sp+0]    guest x0
///   [sp+8]    guest x1
///   ...
///   [sp+160]  ELR_EL1   (restored by the stub before `eret`)
///   [sp+168]  SPSR_EL1
///
/// To pass a value back to the guest we must write into the frame (the stub
/// restores every register from it on return).
#[no_mangle]
pub extern "C" fn sync_handler(esr: u64, elr: u64, _far: u64, sp: u64) {
    let ec = esr_ec(esr);

    match ec {
        EC_HVC64 => {
            // The guest's x0 (function ID) and x1 (argument) are in the
            // saved frame — the stub has already clobbered the live
            // registers with ESR/ELR/etc.
            let frame = sp as *const u64;
            let func = unsafe { core::ptr::read_volatile(frame) };
            let arg1 = unsafe { core::ptr::read_volatile(frame.add(1)) };

            // Dispatch through the doorbell handler.
            // get_backend() returns the same singleton instance as
            // detect_backend().
            let ret = {
                let hv = crate::hypervisor::get_backend();
                crate::hypervisor::doorbell::handle_hvc(func, arg1, hv)
            };

            // Write the return value into the guest's x0 slot.
            unsafe { core::ptr::write_volatile(frame as *mut u64, ret) };

            // Advance the saved ELR past the HVC instruction (4 bytes).
            unsafe { core::ptr::write_volatile(frame.add(20) as *mut u64, elr + 4) };
        }

        EC_DABT_LOWER | EC_IABT_LOWER => {
            let frame = sp as *const u64;
            if from_el0(frame) {
                // An EL0 server faulted — isolate it (Phase 6).
                unsafe { kill_faulting_task(esr, elr, _far) }
            }
            panic!(
                "guest abort EC={:#x} ESR={:#010x} ELR={:#018x} FAR={:#018x}",
                ec, esr, elr, _far
            );
        }

        other => {
            let frame = sp as *const u64;
            if from_el0(frame) {
                // Anything unexpected from EL0 (undefined instruction,
                // alignment, …) is treated as a task fault, not a kernel
                // bug — the server must be quarantined.
                unsafe { kill_faulting_task(esr, elr, _far) }
            }
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
