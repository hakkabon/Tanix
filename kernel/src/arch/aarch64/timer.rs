#![allow(dead_code)]
//! aarch64 generic timer — Phase 1 stub.
//!
//! The ARM Generic Timer provides per-CPU physical and virtual countdown
//! timers.  We use the EL1 Physical Timer (CNTP_*) which is available
//! without EL2/EL3 configuration on QEMU `virt`.
//!
//! Phase 1: initialise the timer and confirm it counts (used for busy-wait
//! delays and future scheduler tick).

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

/// Arm the EL1 physical timer to fire after `ticks` counts.
///
/// The interrupt (GIC IRQ 30 = PPI CNTP) must be enabled in the GIC
/// separately.  This just sets CNTP_TVAL_EL0 and enables the timer.
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
