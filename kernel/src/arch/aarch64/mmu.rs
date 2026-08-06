#![allow(dead_code)]
//! aarch64 MMU initialisation — Phase 1 stub.
//!
//! For Phase 1 the MMU is left *disabled* and the kernel runs with a flat
//! physical address space.  QEMU's `virt` machine starts at EL1 with the MMU
//! off, which is exactly what we need for an early bootstrap.
//!
//! Phase 2 will replace this with a real page-table setup once the frame
//! allocator exists.

/// Reads SCTLR_EL1 and returns true if the MMU (M bit, bit 0) is enabled.
#[inline]
pub fn is_enabled() -> bool {
    let sctlr: u64;
    unsafe {
        core::arch::asm!(
            "mrs {s}, SCTLR_EL1",
            s = out(reg) sctlr,
            options(nomem, nostack)
        );
    }
    sctlr & 1 != 0
}

/// Phase 1 init: configure TCR/MAIR so Phase 2 can enable the MMU without a
/// full re-init, but leave the MMU off for now.
pub fn init() {
    // TCR_EL1: 48-bit VA, 4 KiB granule, inner/outer write-back cacheable.
    //   T0SZ = 16  → VA[47:0] (48-bit user space)
    //   T1SZ = 16  → VA[47:0] (48-bit kernel space)
    //   TG0  = 0b00 → 4 KiB
    //   TG1  = 0b10 → 4 KiB
    //   SH0/SH1 = 0b11 → Inner Shareable
    //   ORGN0/ORGN1 = 0b01 → Outer WB/WA
    //   IRGN0/IRGN1 = 0b01 → Inner WB/WA
    const TCR: u64 = 16          // T0SZ
        | (16 << 16)                    // T1SZ
        | (0b01 << 8)  | (0b01 << 10)  // IRGN0, ORGN0
        | (0b11 << 12)                  // SH0
        | (0b01 << 24) | (0b01 << 26)  // IRGN1, ORGN1
        | (0b11 << 28)                  // SH1
        | (0b10 << 30)                  // TG1 = 4 KiB
        | (1 << 37);                    // TBI0 — ignore top byte tag

    // MAIR_EL1: two memory attribute indices.
    //   Attr0 = 0xFF → Normal, Inner/Outer WB/WA/RA (for DRAM)
    //   Attr1 = 0x00 → Device-nGnRnE (for MMIO)
    const MAIR: u64 = 0x00_FF;

    unsafe {
        core::arch::asm!(
            "msr TCR_EL1,  {tcr}",
            "msr MAIR_EL1, {mair}",
            "isb",
            tcr  = in(reg) TCR,
            mair = in(reg) MAIR,
            options(nomem, nostack)
        );
    }

    // MMU stays off — Phase 2 will call mmu::enable() after building tables.
}
