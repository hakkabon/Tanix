#![allow(dead_code)]
//! GICv3 initialisation — Phase 1 stub.
//!
//! QEMU `virt` exposes a GICv3 at:
//!   Distributor (GICD):  0x0800_0000  (64 KiB)
//!   Redistributors (GICR): 0x080A_0000  (one 128 KiB frame per CPU)
//!
//! Phase 1 performs the minimum setup to unmask IRQs at the CPU interface
//! so that the timer interrupt (Chapter 5) can fire.
//!
//! Phase 2 will extend this to support SGI-based doorbell delivery between VMs.

// ── GICD register offsets ────────────────────────────────────────────────────

const GICD_BASE: usize = 0x0800_0000;
const GICD_CTLR: usize = GICD_BASE;    // Distributor Control Register
const GICD_TYPER: usize = GICD_BASE + 0x004;   // Interrupt Controller Type

// ── GICR register offsets (per-CPU redistributor, CPU 0) ────────────────────

const GICR_BASE: usize = 0x080A_0000;
const GICR_WAKER: usize = GICR_BASE + 0x014;   // Wake register

// ── CPU interface (ICC system registers, GICv3 system register mode) ─────────

#[inline]
fn write_gicd(offset: usize, val: u32) {
    unsafe {
        core::ptr::write_volatile(offset as *mut u32, val);
    }
}

#[inline]
fn read_gicd(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile(offset as *const u32) }
}

#[inline]
fn write_gicr(offset: usize, val: u32) {
    unsafe {
        core::ptr::write_volatile(offset as *mut u32, val);
    }
}

#[inline]
fn read_gicr(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile(offset as *const u32) }
}

/// Initialise GICv3 for CPU 0.
pub fn init() {
    // 1. Wake the redistributor — clear ProcessorSleep (bit 1) and wait
    //    until ChildrenAsleep (bit 2) clears.
    let waker = read_gicr(GICR_WAKER);
    write_gicr(GICR_WAKER, waker & !(1 << 1)); // clear ProcessorSleep
    while read_gicr(GICR_WAKER) & (1 << 2) != 0 {
        core::hint::spin_loop();
    }

    // 2. Enable the distributor (ARE_S, ARE_NS, EnableGrp1S, EnableGrp1NS).
    //    GICD_CTLR[4:0] = 0b10111 enables Group 1 NS and S with Affinity routing.
    write_gicd(GICD_CTLR, 0b10111);

    // 3. Enable the CPU interface via ICC system registers.
    //    ICC_SRE_EL1: enable system register access (SRE bit 0).
    unsafe {
        core::arch::asm!(
            "msr S3_0_C12_C12_5, {sre}",    // ICC_SRE_EL1
            "isb",
            "msr S3_0_C12_C12_7, {igrpen}", // ICC_IGRPEN1_EL1 — enable Group 1 IRQs
            "isb",
            "msr S3_0_C4_C6_0,   {pmr}",    // ICC_PMR_EL1 — priority mask (0xFF = all)
            "isb",
            sre    = in(reg) 1u64,
            igrpen = in(reg) 1u64,
            pmr    = in(reg) 0xFFu64,
            options(nomem, nostack)
        );
    }

    let _ = read_gicd(GICD_TYPER); // read to confirm MMIO is reachable
}

/// Acknowledge the highest-priority pending interrupt.
/// Returns the interrupt ID (1023 = spurious).
#[inline]
pub fn ack() -> u32 {
    let iar: u64;
    unsafe {
        core::arch::asm!(
            "mrs {v}, S3_0_C12_C12_0",  // ICC_IAR1_EL1
            v = out(reg) iar,
            options(nomem, nostack)
        );
    }
    iar as u32
}

/// Signal End-Of-Interrupt for interrupt `id`.
#[inline]
pub fn eoi(id: u32) {
    unsafe {
        core::arch::asm!(
            "msr S3_0_C12_C12_1, {v}",  // ICC_EOIR1_EL1
            v = in(reg) id as u64,
            options(nomem, nostack)
        );
    }
}
