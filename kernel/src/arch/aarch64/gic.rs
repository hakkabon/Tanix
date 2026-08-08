#![allow(dead_code)]
//! GICv3 initialisation and interrupt enablement — Phase 7.
//!
//! QEMU `virt` exposes a GICv3 at:
//!   Distributor (GICD):  0x0800_0000  (64 KiB)
//!   Redistributors (GICR): 0x080A_0000  (one 128 KiB frame per CPU)
//!
//! Phase 1 performed the minimum setup to unmask IRQs at the CPU interface
//! so the timer interrupt could fire.  Phase 7 adds the per-interrupt
//! enablement: the kernel runs non-secure EL1 (SCR_EL3.NS=1), so all
//! SGIs/PPIs/SPIs are configured as **Group 1NS** — Group 0 interrupts are
//! secure-only and would never be delivered.
//!
//! Device IRQs (virtio-mmio: SPIs 48..79) are enabled lazily by the
//! `SYS_WAIT_IRQ` syscall; the EL1 physical timer (PPI 26) is enabled at
//! boot by the scheduler tick setup.

// ── GICD register offsets ────────────────────────────────────────────────────

const GICD_BASE: usize = 0x0800_0000;
const GICD_CTLR: usize = GICD_BASE;    // Distributor Control Register
const GICD_TYPER: usize = GICD_BASE + 0x004;   // Interrupt Controller Type
const GICD_IGROUPR: usize = GICD_BASE + 0x080; // Group 1 enable (bit per INTID)
const GICD_ISENABLER: usize = GICD_BASE + 0x100; // Set-enable (bit per INTID)
const GICD_ICENABLER: usize = GICD_BASE + 0x180; // Clear-enable (bit per INTID)
const GICD_IPRIORITYR: usize = GICD_BASE + 0x400; // Priority (byte per INTID)

// ── GICR register offsets (per-CPU redistributor, CPU 0) ────────────────────

const GICR_BASE: usize = 0x080A_0000;
const GICR_WAKER: usize = GICR_BASE + 0x014;   // Wake register
/// Redistributor SGI/PPI bank (128 KiB into the 256 KiB frame).
const GICR_SGI_BASE: usize = GICR_BASE + 0x1_0000;
const GICR_IGROUPR0: usize = GICR_SGI_BASE + 0x080; // Group 1 for SGIs/PPIs
const GICR_ISENABLER0: usize = GICR_SGI_BASE + 0x104; // Set-enable for SGIs/PPIs
const GICR_ICENABLER0: usize = GICR_SGI_BASE + 0x184; // Clear-enable for SGIs/PPIs

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

/// Enable a PPI (16..31) as a Group 1NS interrupt on the current CPU's
/// redistributor.  Idempotent (set-enable is sticky).
fn enable_ppi(intid: u32) {
    let bit = 1u32 << (intid % 32);
    let igroup = read_gicr(GICR_IGROUPR0) | bit;
    write_gicr(GICR_IGROUPR0, igroup);
    write_gicr(GICR_ISENABLER0, bit);
}

/// Enable an SPI (32..1019) as a Group 1NS interrupt in the distributor.
/// Idempotent.
fn enable_spi(intid: u32) {
    let word = (intid / 32) as usize;
    let bit = 1u32 << (intid % 32);
    let igroup = read_gicd(GICD_IGROUPR + word * 4) | bit;
    write_gicd(GICD_IGROUPR + word * 4, igroup);
    write_gicd(GICD_ISENABLER + word * 4, bit);
    // Default reset priority is 0 (highest) — fine with ICC_PMR = 0xFF.
}

/// Enable any wired interrupt: PPIs 16..31 via the redistributor, SPIs
/// 32..1019 via the distributor.  Group 1NS, so it is delivered to this
/// non-secure EL1 kernel.
pub fn enable_irq(intid: u32) {
    match intid {
        16..=31 => enable_ppi(intid),
        32..=1019 => enable_spi(intid),
        _ => {}
    }
}

/// Disable a wired interrupt (mirror of `enable_irq`).  `irq_handler` uses
/// this for level-triggered device IRQs: after EOI a still-asserted level
/// would re-fire in the single-instruction window before the SYS_WAIT_IRQ
/// wait loop resumes, starving the loop forever.  Disabling the SPI stops
/// the re-fire; the waiter consumes the recorded pending bit and the next
/// `wait_irq` call re-enables the IRQ (level-triggered ⇒ no lost wakeups).
pub fn disable_irq(intid: u32) {
    match intid {
        16..=31 => {
            write_gicr(GICR_ICENABLER0, 1u32 << (intid % 32));
        }
        32..=1019 => {
            let word = (intid / 32) as usize;
            write_gicd(GICD_ICENABLER + word * 4, 1u32 << (intid % 32));
        }
        _ => {}
    }
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
