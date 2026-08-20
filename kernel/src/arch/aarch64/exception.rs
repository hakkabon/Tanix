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
const EC_DABT_LOWER:     u64 = 0x24; // Data abort from lower EL (ARM ARM EC 0x24; 0x25 = current-EL)

// ── IRQ handler ───────────────────────────────────────────────────────────────

/// Called from `vectors.s` slot 5 (current EL/SPx IRQ — `from_el0 == 0`)
/// and slot 9 (lower EL AArch64 IRQ — `from_el0 == 1`).
///
/// `frame` is the base of the full IRQ_ENTRY frame the assembly macro
/// pushed on the interrupted stack (272 bytes: x0-x18, x30, ELR_EL1,
/// SPSR_EL1 and — Phase 21 — x19-x29; offsets in `vectors.s`).
///
/// Acknowledges the interrupt, dispatches it, signals EOI, then lets the
/// scheduler preempt:
///   • slot 9 (EL0 task)                         → `task::tick_preempt`;
///   • slot 5 with a guest vCPU running (Phase 21) → the co-tenant
///     scheduler decrements the tenant's quantum and, on expiry, captures
///     the frame and switches to the next tenant (never returns);
///   • slot 5 inside plain kernel code           → `task::tick_preempt(false)`.
#[no_mangle]
pub extern "C" fn irq_handler(from_el0: u64, frame: *mut u64) {
    use super::gic;
    use crate::sched::task;

    let intid = gic::ack();
    log::debug!("irq: acked INTID={}", intid);

    match intid {
        // PPI 30 — EL1 physical timer (CNTPNSIRQ on QEMU `virt`; the board
        // maps the EL1 timer to INTID 30, the EL2 timer to 26).
        // Phase 7: count + re-arm the tick, then let the scheduler
        // preempt (only when the tick hit an EL0 task — ticks inside the
        // kernel, e.g. the SYS_WAIT_IRQ wait loop, just tick).
        30 => {
            super::timer::tick();
            gic::eoi(intid);
            if from_el0 != 0 {
                unsafe {
                    task::tick_preempt(true);
                }
            } else if crate::hypervisor::backend::guest_tick_active() {
                // Phase 21: tick landed inside a guest vCPU run.  The
                // tenant scheduler consumes the tick; it may preempt the
                // guest (never returns) or leave it running.
                unsafe {
                    crate::hypervisor::backend::tick_guest(frame);
                }
            } else {
                unsafe {
                    task::tick_preempt(false);
                }
            }
        }

        // SGIs 1/2 — inter-VM doorbells (Phase 14).  `doorbell_send` rings
        // these SGIs; every *delivery* lands here, so bursts of rings
        // coalesce into one IRQ — recorded by `note_delivery` (rings vs.
        // deliveries is observable in the doorbell stats).  Mirrors the
        // doorbell vIRQ a Gunyah guest would receive on its vCPU.
        1 | 2 => {
            crate::hypervisor::doorbell::note_delivery(intid);
            gic::eoi(intid);
        }

        // SGI 0 — used for IPI / reschedule in SMP (Phase 4).
        0 => {
            log::trace!("irq: SGI 0 (reschedule)");
            gic::eoi(intid);
        }

        // SGI 3 — run-queue poke (Phase 11): sent by the device-IRQ handler
        // and by wake sites when a parked secondary / `SYS_WAIT_IRQ` waiter
        // sleeps in `wfi` on another core.  Waking it is the whole job —
        // the core re-checks the runqueue / pending bit on its own.
        3 => {
            log::trace!("irq: SGI 3 (run-queue poke)");
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
            // Phase 11: a waiter may be parked in `wfi` on another core —
            // the pending bit is useless to it unless it is woken.
            unsafe {
                crate::sched::task::poke_other_cpus();
            }
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
///
/// Phase 21: takes the already-snapshot *value* instead of re-reading the
/// shared frame from a raw pointer — `sync_handler` copies the frame's
/// words it needs into locals, and `from_el0` derives its answer from that
/// snapshot, so no aliasing `*const`/`*mut` reads ever touch the same
/// memory location.
#[inline]
fn from_el0(spsr: u64) -> bool {
    spsr & 0xF == 0
}

/// Abort from an EL0 task: log, mark it zombie and switch away.  Never
/// returns.  The kernel itself (and the EL1 guest) are unaffected — this
/// is the Phase-6 isolation guarantee.
///
/// Phase 11: takes `SCHED_LOCK`; the switch releases it between saving
/// this context and restoring the next one.  The zombie is never re-picked.
unsafe fn kill_faulting_task(esr: u64, elr: u64, far: u64) -> ! {
    use crate::sched::task::{context_switch_unlock, scheduler};
    use crate::sched::TaskState;

    let lock = crate::sched::task::sched_lock();
    lock.lock();
    let sched = scheduler();
    let cpu = crate::smp::cpu_index();
    let idx = crate::smp::current_idx();
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

    // Switch to the next runnable task (this core's idle slot when nothing
    // else is runnable); the zombie never runs again.
    let next = sched.pick_next(cpu);
    let from = sched.ctx_ptr(idx);
    let to = sched.ctx_ptr(next);
    sched.set_state(next, TaskState::Running);
    crate::smp::set_current(next);
    context_switch_unlock(lock, from, to);
    unreachable!("kill_faulting_task resumed a zombie")
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
            // The guest's x0-x3 (function ID + arguments) are in the saved
            // frame — the stub has already clobbered the live registers
            // with ESR/ELR/etc.  Snapshot the words we need into locals
            // (Phase 21: no long-lived raw alias of the shared frame).
            let frame = sp as *const u64;
            let mut args = [0u64; 4];
            unsafe {
                core::ptr::copy_nonoverlapping(frame, args.as_mut_ptr(), 4);
            }

            // Dispatch through the VMM service handler.
            // get_backend() returns the same singleton instance as
            // detect_backend().
            let ret = {
                let hv = crate::hypervisor::get_backend();
                crate::hypervisor::doorbell::handle_hvc(args, hv)
            };

            // Write the return value into the guest's x0 slot.
            unsafe { core::ptr::write_volatile(frame as *mut u64, ret) };

            // Advance the saved ELR past the HVC instruction (4 bytes).
            unsafe { core::ptr::write_volatile(frame.add(20) as *mut u64, elr + 4) };
        }

        EC_DABT_LOWER => {
            let frame = sp as *const u64;
            let spsr = unsafe { core::ptr::read_volatile(frame.add(21)) };
            if from_el0(spsr) {
                // Phase 19: offer the fault to the VM-fault resolver first
                // (demand paging / COW / stack growth).  On success the
                // faulting instruction re-executes untouched; otherwise the
                // task is isolated (Phase 6).
                if crate::mem::vm_fault::resolve_user_fault(_far as usize, esr) {
                    return;
                }
                unsafe { kill_faulting_task(esr, elr, _far) }
            } else if crate::hypervisor::backend::guest_tick_active() {
                // Phase 21: a co-tenant guest (EL1, same-level as the kernel)
                // dereferenced a bad address — the fault lands in OUR sync
                // vector with (ELR, FAR) = the guest's faulting state.  The
                // kernel must not die with the tenant: park the guest in its
                // info block (PARKED → the scheduler drops it) and park this
                // CPU until then.  Report the fault once, raw-atomic.
                raw_uart_line("GUESTfault ESR=", esr);
                raw_uart_line("GUESTfault ELR=", elr);
                raw_uart_line("GUESTfault FAR=", _far);
                unsafe {
                    crate::hypervisor::backend::park_active_tenant();
                    loop {
                        core::arch::asm!("wfi", options(nomem, nostack));
                    }
                }
            }
            panic!(
                "guest abort EC={:#x} ESR={:#010x} ELR={:#018x} FAR={:#018x}",
                ec, esr, elr, _far
            );
        }

        other => {
            let frame = sp as *const u64;
            let spsr = unsafe { core::ptr::read_volatile(frame.add(21)) };
            if from_el0(spsr) {
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
///
/// Guards against recursive panics (see the WFI re-entry park at the top).
static mut PANICKING: bool = false;

/// Raw UART dump helper — no fmt, no log, no stack machinery.  Writes to
/// the machine EL1 console directly so a faulting page or a broken panic
/// path can still report the (ESR, ELR, FAR, descriptor) quadruple.
fn raw_uart_hex(v: u64) {
    let dr = crate::arch::aarch64::machine().uart_base as *mut u32;
    let fr = (crate::arch::aarch64::machine().uart_base + 0x18) as *const u32;
    let hex = b"0123456789abcdef";
    unsafe {
        for i in (0..16).rev() {
            while core::ptr::read_volatile(fr) & (1 << 5) != 0 {
                core::hint::spin_loop();
            }
            core::ptr::write_volatile(dr, hex[((v >> (i * 4)) & 0xf) as usize] as u32);
        }
    }
}

fn raw_uart_str(s: &str) {
    let dr = crate::arch::aarch64::machine().uart_base as *mut u32;
    let fr = (crate::arch::aarch64::machine().uart_base + 0x18) as *const u32;
    unsafe {
        for &b in s.as_bytes() {
            while core::ptr::read_volatile(fr) & (1 << 5) != 0 {
                core::hint::spin_loop();
            }
            core::ptr::write_volatile(dr, b as u32);
        }
    }
}

fn raw_uart_line(tag: &str, v: u64) {
    raw_uart_str(tag);
    raw_uart_hex(v);
    raw_uart_str("\n");
}

#[no_mangle]
pub extern "C" fn exception_handler(kind: u64, esr: u64, elr: u64, far: u64, frame: u64) -> ! {
    // Re-entry guard FIRST: a faulting panic path (or this dump's own
    // table walk on a garbage FAR) must park instead of flooding.
    unsafe {
        if *core::ptr::addr_of!(PANICKING) {
            loop {
                core::arch::asm!("wfi", options(nomem, nostack));
            }
        }
        core::ptr::write_volatile(&mut PANICKING, true);
    }
    raw_uart_str("\n!!EXC ");
    raw_uart_str(match kind {
        4 => "cur-sync",
        5 => "cur-irq",
        9 => "low-irq",
        k => {
            raw_uart_line("k=", k);
            "?"
        }
    });
    raw_uart_line("ESR=", esr);
    raw_uart_line("ELR=", elr);
    raw_uart_line("FAR=", far);
    let (ttbr0, tcr, sctlr, cur, spsr_el1): (u64, u64, u64, u64, u64);
    unsafe {
        core::arch::asm!(
            "mrs {a}, TTBR0_EL1",
            "mrs {b}, TCR_EL1",
            "mrs {c}, SCTLR_EL1",
            "mrs {d}, CurrentEL",
            "mrs {e}, SPSR_EL1",
            a = out(reg) ttbr0,
            b = out(reg) tcr,
            c = out(reg) sctlr,
            d = out(reg) cur,
            e = out(reg) spsr_el1,
            options(nomem, nostack)
        );
    }
    raw_uart_line("EL=", cur >> 2);
    raw_uart_line("TTBR0=", ttbr0);
    // The 272-byte exception frame (`EXCEPTION_ENTRY` in vectors.s) holds
    // the faulting context's register file: x10@80, x30@152, ELR@160,
    // SPSR@168.  For a co-tenant guest fault this is the guest's own file —
    // the only way to see why its data pointer went bad.
    unsafe {
        let fr = frame as *const u64;
        raw_uart_line("gX10=", core::ptr::read_volatile(fr.add(10)));
        raw_uart_line("gX12=", core::ptr::read_volatile(fr.add(12)));
        raw_uart_line("gX13=", core::ptr::read_volatile(fr.add(13)));
        raw_uart_line("gX30=", core::ptr::read_volatile(fr.add(19)));
        raw_uart_line("gELR=", core::ptr::read_volatile(fr.add(20)));
        raw_uart_line("gSPSR=", core::ptr::read_volatile(fr.add(21)));
    }
    let l0 = (ttbr0 & 0x0000_FFFF_FFFF_F000) as *const u64;
    let (l0e, l1e, l2e, l3e): (u64, u64, u64, u64);
    unsafe {
        l0e = core::ptr::read_volatile(l0.add(((far >> 39) & 0x1FF) as usize));
        let l1 = (l0e & 0x0000_FFFF_FFFF_F000) as *const u64;
        l1e = if l0e & 3 == 3 {
            core::ptr::read_volatile(l1.add(((far >> 30) & 0x1FF) as usize))
        } else {
            0
        };
        let l2 = (l1e & 0x0000_FFFF_FFFF_F000) as *const u64;
        l2e = if l1e & 3 == 3 {
            core::ptr::read_volatile(l2.add(((far >> 21) & 0x1FF) as usize))
        } else {
            0
        };
        let l3 = (l2e & 0x0000_FFFF_FFFF_F000) as *const u64;
        l3e = if l2e & 3 == 3 {
            core::ptr::read_volatile(l3.add(((far >> 12) & 0x1FF) as usize))
        } else {
            0
        };
    }
    raw_uart_line("L0=", l0e);
    raw_uart_line("L1=", l1e);
    raw_uart_line("L2=", l2e);
    raw_uart_line("L3=", l3e);
    raw_uart_line("SCTLR=", sctlr);
    panic!(
        "fatal exception kind={} ESR={:#010x} ELR={:#018x} FAR={:#018x}",
        kind, esr, elr, far
    );
}
