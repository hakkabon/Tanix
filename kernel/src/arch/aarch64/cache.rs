//! Cache maintenance and barrier operations — Phase 16.
//!
//! QEMU does not model caches, so the Phase-1..15 kernel never needed
//! any of this.  Real hardware does, and getting the MMU handover wrong
//! there is a classic silent-corruption bug:
//!
//!   • page-table writes must be made globally visible before the MMU
//!     walks them (DSB ISH), and the TLB must be flushed after the walk
//!     tables change (TLBI + DSB + ISB);
//!   • stale I-cache lines can resurrect old code after mapping changes
//!     (`ic iallu` before enabling the MMU / after installing code);
//!   • DMA requires explicit maintenance: the CPU must clean lines to the
//!     Point of Coherence before a device reads them (`dc cvau`) and
//!     invalidate lines before a device's writes become visible to the
//!     CPU (`dc ivau`).  QEMU's emulated devices share the guest's memory
//!     model, so these are no-ops there — but they are required on real
//!     silicon and exercised by `SYS_CACHE_SYNC` (the net server's virtio
//!     rings) and by the EL3 monitor (world-switch handoff).
//!
//! At EL1 only by-address operations are available (`dc` by set/way is
//! EL2/EL3-only on AArch64); the I-cache, however, may be invalidated
//! wholesale with `ic iallu`.

#![allow(dead_code)]

// ── Barriers ──────────────────────────────────────────────────────────────────

/// Full system barrier (all reads/writes, all observers).
#[inline]
pub fn dsb_sy() {
    unsafe { core::arch::asm!("dsb sy", options(nomem, nostack)) };
}

/// Inner-shareable barrier — the scope of our page tables and IPIs.
#[inline]
pub fn dsb_ish() {
    unsafe { core::arch::asm!("dsb ish", options(nomem, nostack)) };
}

/// Outer-shareable barrier (DMA / device-visible data).
#[inline]
pub fn dsb_osh() {
    unsafe { core::arch::asm!("dsb osh", options(nomem, nostack)) };
}

/// Data memory barrier (ordering only, no completion).
#[inline]
pub fn dmb_ish() {
    unsafe { core::arch::asm!("dmb ish", options(nomem, nostack)) };
}

/// Instruction synchronization barrier — flushes the pipeline / prefetch
/// after system-register or mapping changes.
#[inline]
pub fn isb() {
    unsafe { core::arch::asm!("isb", options(nomem, nostack)) };
}

// ── Cache maintenance (by virtual address, to the Point of Coherence) ────────

/// Clean (write back) one cache line by VA — the VA is ignored in QEMU
/// and on virtually-indexed caches the line is found via the address bits.
/// Required before a device/DMA reads memory the CPU wrote.
#[inline]
pub fn clean_dcache_line(va: usize) {
    unsafe {
        core::arch::asm!("dc cvau, {v}", v = in(reg) va, options(nomem, nostack));
    }
}

/// Invalidate one cache line by VA.  Required before the CPU reads memory
/// a device/DMA wrote.  (Safe on real hardware because we only ever
/// invalidate ranges whose lines were previously cleaned or are known
/// stale.)
#[inline]
pub fn invalidate_dcache_line(va: usize) {
    unsafe {
        core::arch::asm!("dc ivau, {v}", v = in(reg) va, options(nomem, nostack));
    }
}

/// Clean and invalidate one line by VA — the full handover for a shared
/// buffer.
#[inline]
pub fn clean_invalidate_dcache_line(va: usize) {
    unsafe {
        core::arch::asm!("dc civac, {v}", v = in(reg) va, options(nomem, nostack));
    }
}

/// Invalidate the whole I-cache (the only full-cache op available at EL1).
#[inline]
pub fn invalidate_icache_all() {
    unsafe {
        core::arch::asm!("ic iallu", "isb", options(nomem, nostack));
    }
}

/// Flush the TLB for the whole stage-1 translation (all ASIDs, inner
/// shareable) and synchronize.
#[inline]
pub fn tlb_flush_all() {
    unsafe {
        core::arch::asm!("tlbi vmalle1is", "dsb ish", "isb", options(nomem, nostack));
    }
}

// ── Range helpers ─────────────────────────────────────────────────────────────

/// Clean `[va, va + size)` so the device sees every byte.  The range is
/// rounded out to cache lines (64 bytes on all supported CPUs).
pub fn clean_dcache_range(va: usize, size: usize) {
    if size == 0 {
        return;
    }
    let mut a = va & !63;
    let end = (va + size + 63) & !63;
    while a < end {
        clean_dcache_line(a);
        a += 64;
    }
    dsb_osh();
}

/// Invalidate `[va, va + size)` so the CPU re-reads device-written data.
pub fn invalidate_dcache_range(va: usize, size: usize) {
    if size == 0 {
        return;
    }
    let mut a = va & !63;
    let end = (va + size + 63) & !63;
    while a < end {
        invalidate_dcache_line(a);
        a += 64;
    }
    dsb_osh();
}

/// Clean then invalidate `[va, va + size)`.
pub fn clean_invalidate_dcache_range(va: usize, size: usize) {
    clean_dcache_range(va, size);
    invalidate_dcache_range(va, size);
}

// ── MMU handover sequence ─────────────────────────────────────────────────────
//
// The canonical order for turning the MMU on (ARM DDI 0487 "B2.3.5"):

/// Barrier + I-cache + TLB primitives that must run *before* SCTLR_EL1.M
/// is set: any pending table writes are made visible, stale TLB entries
/// are dropped while the MMU is off (cheap), and the I-cache is purged so
/// no stale code can be fetched from the newly-mapped image.
pub fn before_mmu_enable() {
    dsb_ish();
    tlb_flush_all();
    invalidate_icache_all();
}

/// After SCTLR_EL1.M is set: re-flush the TLB (the walker may have filled
/// entries during the enable sequence itself) and synchronize.
pub fn after_mmu_enable() {
    tlb_flush_all();
    dsb_ish();
    isb();
}
