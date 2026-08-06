#![allow(dead_code)]
//! AArch64 4-level page tables (VA[47:0], 4 KiB granule).
//!
//! Table hierarchy:
//!   Level 0 (PGD) — VA[47:39]  512 entries  512 GiB per entry
//!   Level 1 (PUD) — VA[38:30]  512 entries    1 GiB per entry
//!   Level 2 (PMD) — VA[29:21]  512 entries    2 MiB per entry
//!   Level 3 (PTE) — VA[20:12]  512 entries    4 KiB per entry
//!
//! For Phase 2 we build a minimal kernel identity map (PA == VA) that covers:
//!   • the kernel image     (normal, RWX temporarily, tightened in Phase 4)
//!   • GIC MMIO             (device-nGnRnE)
//!   • UART MMIO            (device-nGnRnE)
//!
//! Each table is exactly one 4 KiB page (512 × 8-byte entries).

use super::{PhysAddr, VirtAddr, PAGE_SIZE};
use super::frame::alloc_frame;

// ── Descriptor flags ─────────────────────────────────────────────────────────

/// Bit 0: valid descriptor.
pub const DESC_VALID: u64 = 1 << 0;
/// Bit 1: table (intermediate) descriptor when set; block descriptor when clear.
pub const DESC_TABLE: u64 = 1 << 1;
/// Bit 1: page descriptor at level 3.
pub const DESC_PAGE: u64 = 1 << 1;

// Access flag — must be set or the hardware raises an Access Flag Fault.
pub const DESC_AF: u64 = 1 << 10;

// Shareability
pub const DESC_SH_INNER: u64 = 0b11 << 8;

// AP[2:1] — access permissions
pub const DESC_AP_RW_EL1: u64 = 0b00 << 6; // R/W at EL1, no access EL0
pub const DESC_AP_RO_EL1: u64 = 0b10 << 6; // R/O at EL1, no access EL0

// UXN / PXN — execute-never bits
pub const DESC_UXN: u64 = 1 << 54;
pub const DESC_PXN: u64 = 1 << 53;

// MAIR index (matches MAIR_EL1 programmed in mmu.rs)
pub const ATTR_NORMAL: u64 = 0 << 2; // index 0 = Normal WB/WA
pub const ATTR_DEVICE: u64 = 1 << 2; // index 1 = Device-nGnRnE

/// A complete set of page-table descriptor flags for a normal kernel page.
pub const FLAGS_KERNEL_RWX: u64 = DESC_VALID
    | DESC_PAGE
    | DESC_AF
    | DESC_SH_INNER
    | DESC_AP_RW_EL1
    | ATTR_NORMAL;

/// Read-only kernel page (for .rodata sections — Phase 4).
pub const FLAGS_KERNEL_RO: u64 = FLAGS_KERNEL_RWX | DESC_PXN | DESC_UXN;

/// Device MMIO mapping: non-executable, non-cacheable.
pub const FLAGS_DEVICE: u64 = DESC_VALID
    | DESC_PAGE
    | DESC_AF
    | DESC_AP_RW_EL1
    | DESC_UXN
    | DESC_PXN
    | ATTR_DEVICE;

// ── Table type ────────────────────────────────────────────────────────────────

/// A single page-table level — 512 × 8-byte entries.
#[repr(C, align(4096))]
pub struct PageTable {
    entries: [u64; 512],
}

impl PageTable {
    pub const fn empty() -> Self {
        Self { entries: [0u64; 512] }
    }

    pub fn zero(&mut self) {
        for e in self.entries.iter_mut() {
            *e = 0;
        }
    }

    #[inline]
    pub fn entry(&self, idx: usize) -> u64 {
        self.entries[idx]
    }

    #[inline]
    pub fn set_entry(&mut self, idx: usize, val: u64) {
        self.entries[idx] = val;
    }
}

// ── Index helpers ─────────────────────────────────────────────────────────────

#[inline] pub fn l0_idx(va: VirtAddr) -> usize { (va >> 39) & 0x1FF }
#[inline] pub fn l1_idx(va: VirtAddr) -> usize { (va >> 30) & 0x1FF }
#[inline] pub fn l2_idx(va: VirtAddr) -> usize { (va >> 21) & 0x1FF }
#[inline] pub fn l3_idx(va: VirtAddr) -> usize { (va >> 12) & 0x1FF }

// ── Root table ────────────────────────────────────────────────────────────────

/// The kernel's root (L0) page table.
///
/// This is a static so it does not occupy heap space before the allocator
/// is ready.  All other levels are allocated dynamically via the frame
/// allocator.
static mut KERNEL_L0: PageTable = PageTable::empty();

/// Return the physical address of the kernel L0 table.
pub fn kernel_l0_phys() -> PhysAddr {
    core::ptr::addr_of!(KERNEL_L0) as PhysAddr
}

// ── Mapping helpers ───────────────────────────────────────────────────────────

/// Allocate and zero a new page table frame.
///
/// # Safety
/// Frame allocator must be initialised.
unsafe fn alloc_table() -> *mut PageTable {
    let phys = alloc_frame().expect("page table: out of memory");
    let ptr = phys as *mut PageTable;
    (*ptr).zero();
    ptr
}

/// Ensure a table entry at `table[idx]` points to a next-level table.
/// If the entry is empty, allocate a new table.
/// Returns a pointer to the next-level table.
unsafe fn ensure_table(table: *mut PageTable, idx: usize) -> *mut PageTable {
    let entry = (*table).entry(idx);
    if entry & DESC_VALID != 0 {
        // Already present — extract the physical address from bits [47:12].
        ((entry & 0x0000_FFFF_FFFF_F000) as usize) as *mut PageTable
    } else {
        let child = alloc_table();
        let child_phys = child as u64;
        (*table).set_entry(idx, child_phys | DESC_VALID | DESC_TABLE);
        child
    }
}

/// Map a single 4 KiB page: VA `vaddr` → PA `paddr` with `flags`.
///
/// # Safety
/// Frame allocator must be initialised.  `vaddr` and `paddr` must be
/// 4 KiB-aligned.
pub unsafe fn map_page(vaddr: VirtAddr, paddr: PhysAddr, flags: u64) {
    debug_assert_eq!(vaddr & (PAGE_SIZE - 1), 0, "map_page: vaddr unaligned");
    debug_assert_eq!(paddr & (PAGE_SIZE - 1), 0, "map_page: paddr unaligned");

    let l0 = core::ptr::addr_of_mut!(KERNEL_L0);
    let l1 = ensure_table(l0, l0_idx(vaddr));
    let l2 = ensure_table(l1, l1_idx(vaddr));
    let l3 = ensure_table(l2, l2_idx(vaddr));

    (*l3).set_entry(l3_idx(vaddr), (paddr as u64) | flags);
}

/// Map a contiguous physical range [paddr .. paddr + size) to the same
/// virtual range (identity map) with the given `flags`.
///
/// `size` is rounded up to the nearest page.
///
/// # Safety
/// Same requirements as `map_page`.
pub unsafe fn map_range(paddr: PhysAddr, size: usize, flags: u64) {
    let pages = size.div_ceil(PAGE_SIZE);
    for i in 0..pages {
        let offset = i * PAGE_SIZE;
        map_page(paddr + offset, paddr + offset, flags);
    }
}

// ── MMU enable ────────────────────────────────────────────────────────────────

/// Build the kernel identity map and enable the MMU.
///
/// After this call all physical addresses equal their virtual addresses.
/// MMIO regions are mapped as Device memory; DRAM as Normal WB.
///
/// # Safety
/// Must be called exactly once, after the frame allocator is initialised.
pub unsafe fn enable(kernel_end: PhysAddr) {
    // 1. Identity-map the kernel image (normal memory, RWX).
    //    From RAM_START (0x4000_0000) to kernel_end, rounded to 2 MiB so we
    //    don't partially map a large page.
    let kern_size = (kernel_end - 0x4000_0000 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    map_range(0x4000_0000, kern_size, FLAGS_KERNEL_RWX);

    // 2. Map GICv3 distributor + redistributor (device).
    map_range(0x0800_0000, 2 * 1024 * 1024, FLAGS_DEVICE); // 2 MiB covers GICD + GICR

    // 3. Map PL011 UART (device).
    map_range(0x0900_0000, PAGE_SIZE, FLAGS_DEVICE);

    // 4. Install TTBR0_EL1 (user/low address space) — we use TTBR0 for
    //    the identity map since our kernel VAs are below 0x0001_0000_0000.
    let ttbr0 = kernel_l0_phys() as u64;
    core::arch::asm!(
        "msr TTBR0_EL1, {t}",
        "isb",
        t = in(reg) ttbr0,
        options(nomem, nostack)
    );

    // 5. Flush TLB (all entries, all ASID).
    core::arch::asm!("tlbi vmalle1", "dsb sy", "isb", options(nomem, nostack));

    // 6. Enable MMU: set SCTLR_EL1.M (bit 0) and I-cache (bit 12).
    let mut sctlr: u64;
    core::arch::asm!("mrs {s}, SCTLR_EL1", s = out(reg) sctlr, options(nomem, nostack));
    sctlr |= 1 | (1 << 12); // M | I
    core::arch::asm!(
        "msr SCTLR_EL1, {s}",
        "isb",
        s = in(reg) sctlr,
        options(nomem, nostack)
    );

    log::info!("MMU enabled — identity map active");
}
