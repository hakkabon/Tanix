//! aarch64 boot utilities.
//!
//! Helpers for reading CPU state at boot time (e.g. current exception level,
//! MPIDR for core identification).  The actual entry sequence lives in
//! `_start` / `kmain` in `main.rs`; this module provides the supporting
//! primitives.

#![allow(dead_code)]

/// Returns the current exception level (0–3).
#[inline]
pub fn current_el() -> u8 {
    let el: u64;
    unsafe {
        core::arch::asm!(
            "mrs {el}, CurrentEL",
            el = out(reg) el,
            options(nomem, nostack)
        );
    }
    // CurrentEL[3:2] holds the EL value; bits [1:0] are always 0.
    ((el >> 2) & 0x3) as u8
}

/// Returns the MPIDR_EL1 value (used to identify the current CPU core).
#[inline]
pub fn mpidr() -> u64 {
    let val: u64;
    unsafe {
        core::arch::asm!(
            "mrs {v}, MPIDR_EL1",
            v = out(reg) val,
            options(nomem, nostack)
        );
    }
    val
}

/// Returns `true` if this is the primary (boot) CPU (Aff0 == 0, Aff1 == 0).
#[inline]
pub fn is_primary_cpu() -> bool {
    mpidr() & 0xFF_FF == 0
}
