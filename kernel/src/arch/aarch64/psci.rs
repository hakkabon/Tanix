//! PSCI calls over the SMC conduit — Phase 11 (SMP secondary bring-up).
//!
//! QEMU's `virt` machine emulates PSCI in its (fake) EL3 firmware when EL3
//! is enabled (the default with `-machine virt`); the guest invokes it with
//! `smc #0` from any NS exception level.  The kernel runs at EL1, so the
//! SMC traps straight to QEMU's PSCI handler.
//!
//! PSCI v1.1 (QEMU 11.0.3, `target/arm/tcg/psci.c`): a `CPU_ON` target is
//! started at the highest enabled exception level (EL2 on this machine) in
//! the calling CPU's execution mode (AArch64), with `entry` in PC and
//! `context_id` in x0.

#![allow(dead_code)]

/// 64-bit CPU_ON (PSCI 0.2+, AArch64 calling convention).
pub const FN64_CPU_ON: u64 = 0xC400_0003;

/// Power a secondary CPU on: it starts executing `entry` at EL2 (QEMU).
///
/// Returns PSCI status: 0 = SUCCESS, negative errno otherwise
/// (e.g. INVALID_PARAMS when the target CPU does not exist).
pub fn cpu_on(mpidr: u64, entry: u64, context_id: u64) -> i32 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "smc #0",
            in("x0") FN64_CPU_ON,
            in("x1") mpidr,
            in("x2") entry,
            in("x3") context_id,
            lateout("x0") ret,
            options(nostack)
        );
    }
    ret as i32
}
