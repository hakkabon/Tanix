#![allow(dead_code)]
//! aarch64 generic timer — Phase 7 scheduler tick.
//!
//! The ARM Generic Timer provides per-CPU physical and virtual countdown
//! timers.  We use the EL1 Physical Timer (CNTP_*), which is available
//! without EL2/EL3 configuration on QEMU `virt`.
//!
//! Phase 1: initialise the timer and confirm it counts (used for busy-wait
//! delays).
//!
//! Phase 7: the timer is the preemption source.  `init_tick` grants EL1
//! access to the CNTP registers (CNTKCTL_EL1), enables the physical timer
//! interrupt in the GIC (**PPI 30** — CNTPNSIRQ on QEMU `virt`) and arms a
//! periodic 1 ms tick.  Every tick fires the current-EL / lower-EL IRQ
//! vector and the scheduler decides whether to preempt.

use core::sync::atomic::{AtomicU64, Ordering};

/// Read the current counter frequency (Hz) from CNTFRQ_EL0.
#[inline]
pub fn frequency() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!(
            "mrs {f}, CNTFRQ_EL0",
            f = out(reg) freq,
            options(nomem, nostack)
        );
    }
    freq
}

/// Read the current physical count value.
#[inline]
pub fn read_count() -> u64 {
    let cnt: u64;
    unsafe {
        core::arch::asm!(
            "mrs {c}, CNTPCT_EL0",
            c = out(reg) cnt,
            options(nomem, nostack)
        );
    }
    cnt
}

/// Busy-wait for approximately `ms` milliseconds.
///
/// Accuracy depends on CNTFRQ_EL0 being set correctly by firmware/QEMU
/// (typically 62.5 MHz on the `virt` machine → 1 ms = 62_500 ticks).
pub fn busy_wait_ms(ms: u64) {
    let freq = frequency();
    let ticks = freq / 1000 * ms;
    let start = read_count();
    while read_count().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// Milliseconds per scheduler tick (the preemption quantum).
pub const TICK_PERIOD_MS: u64 = 1;

/// Number of ticks delivered since `init_tick` (informational / tests).
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Total ticks delivered so far.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Arm the EL1 physical timer to fire after `ticks` counts.
///
/// The interrupt (GIC PPI 30) must be enabled separately.  This just sets
/// CNTP_TVAL_EL0 and enables the timer.
pub fn arm(ticks: u64) {
    unsafe {
        core::arch::asm!(
            "msr CNTP_TVAL_EL0, {tval}", // set countdown value
            "msr CNTP_CTL_EL0,  {ctl}",  // enable timer, unmask interrupt
            "isb",
            tval = in(reg) ticks,
            ctl  = in(reg) 1u64,         // ENABLE=1, IMASK=0
            options(nomem, nostack)
        );
    }
}

/// Arm the periodic tick: `TICK_PERIOD_MS` from now.
pub fn arm_next() {
    arm(frequency() / 1000 * TICK_PERIOD_MS);
}

/// Disarm / mask the EL1 physical timer.
pub fn disarm() {
    unsafe {
        core::arch::asm!(
            "msr CNTP_CTL_EL0, {ctl}",
            "isb",
            ctl = in(reg) 0b10u64,   // ENABLE=0, IMASK=1
            options(nomem, nostack)
        );
    }
}

/// Initialise the timer subsystem.
/// For Phase 1 this just disarms the timer so it doesn't fire spuriously.
pub fn init() {
    disarm();
}

/// Start the Phase-7 preemption tick.
///
/// 1. Grants EL1 access to the CNTP registers (CNTKCTL_EL1.ENPCT/ENPTC/
///    ENVTC) — without ENPTC the `msr CNTP_TVAL_EL0` above would trap.
/// 2. Arms the periodic 1 ms tick.  The interrupt must already be enabled
///    in the GIC (`gic::enable_irq(30)`).
pub fn init_tick() {
    unsafe {
        core::arch::asm!(
            "msr cntkctl_el1, {ctl}",
            "isb",
            ctl = in(reg) 0b111u64, // ENPCT | ENPTC | ENVTC
            options(nomem, nostack)
        );
    }
    TICKS.store(0, Ordering::Relaxed);
    arm_next();

    // Boot self-test: does the timer actually assert?  Arm for 10 ms and
    // wait for ISTATUS with IRQs masked — if this never fires, the CNTP
    // interrupt line is not reaching the GIC.
    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack)); // mask IRQ
        arm(frequency() / 100);
        let t0 = read_count();
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
        let t1 = read_count();
        let st: u64;
        core::arch::asm!("mrs {s}, CNTP_CTL_EL0", s = out(reg) st, options(nomem, nostack));
        log::info!(
            "timer: self-test — CNTPCT delta={} over 100k spins, CNTP_CTL_EL0={:#x}",
            t1.saturating_sub(t0),
            st
        );
        let deadline = read_count() + frequency() / 100;
        let mut ok = false;
        while read_count() < deadline {
            let st: u64;
            core::arch::asm!("mrs {s}, CNTP_CTL_EL0", s = out(reg) st, options(nomem, nostack));
            if st & (1 << 2) != 0 {
                ok = true;
                break;
            }
        }
        if ok {
            log::info!("timer: self-test — CNTP asserts (IRQ line works)");
        } else {
            log::error!("timer: self-test — CNTP never asserted ISTATUS");
        }
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack)); // unmask IRQ
        arm_next();
    }

    log::info!("timer: preemption tick armed ({} ms, {} Hz)", TICK_PERIOD_MS, frequency() / 1000 * 1000 / TICK_PERIOD_MS);
}

/// Called from the IRQ handler on every physical-timer interrupt: count it
/// and re-arm the next tick.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    arm_next();
}
