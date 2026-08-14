#![allow(dead_code)]
//! aarch64 MMU initialisation — Phase 1 stub + Phase 16 hardening.
//!
//! TCR/MAIR are programmed at boot with the MMU left off; `page_table::enable`
//! builds the identity map and turns the MMU on.  Phase 16 adds the cache
//! and barrier discipline real hardware requires around the handover
//! (see `cache.rs`).

use super::cache;

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
    //   IPS  = min(PARANGE, 48-bit).  IPS must cover the whole physical
    //   address space or every walk above the cap fails with an
    //   Address-Size fault — sbsa-ref RAM starts at 0x10000000000 (bit 40)
    //   with the page tables just above it, so anything below 42 bits
    //   faults.  Derive it from ID_AA64MMFR0_EL1.PARANGE so the same
    //   binary is valid on QEMU (a57 PARANGE = 44-bit) and on any board
    //   that can physically host this 1 TiB memory layout.
    let ips: u64;
    unsafe {
        core::arch::asm!("mrs {r}, ID_AA64MMFR0_EL1", r = out(reg) ips, options(nomem, nostack));
    }
    // PARANGE (ID_AA64MMFR0<3:0>): 0b000=32, 0b001=36, 0b010=40, 0b011=42,
    // 0b100=44, 0b101=48, 0b110=52 bits.  Clamp to [42, 48] — anything
    // below 42 bits cannot address this machine's RAM at all.
    let ips_field = ((ips >> 0) & 0xF).clamp(0b011, 0b101);
    let tcr: u64 = 16          // T0SZ
        | (16 << 16)                    // T1SZ
        | (ips_field << 32)             // IPS — physical address size
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
            tcr  = in(reg) tcr,
            mair = in(reg) MAIR,
            options(nomem, nostack)
        );
    }
    // Real hardware: make the system-register writes take effect before
    // anything (including the MMU enable) observes them.
    cache::dsb_ish();
    cache::isb();

    // MMU stays off — Phase 2 will call mmu::enable() after building tables.
}
