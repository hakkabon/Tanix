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
/// R/W at EL1 and EL0 — the EL0 task's own pages (Phase 6).
pub const DESC_AP_RW_EL0: u64 = 0b01 << 6;

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

// ── Block descriptor flags (L2, 2 MiB) ───────────────────────────────────────
//
// Block descriptors must NOT set bit 1 (that marks a table descriptor).
// We use them to pre-map large ranges (entire DDR, MMIO windows) before the
// MMU is enabled, so every later frame-allocation is already mapped and no
// page-table frames need to be allocated after the MMU is live.

/// Normal (WB/WA cacheable) 2 MiB block, RW at EL1.
pub const FLAGS_BLOCK_NORMAL: u64 = DESC_VALID
    | DESC_AF
    | DESC_SH_INNER
    | DESC_AP_RW_EL1
    | ATTR_NORMAL;

/// Device-nGnRnE 2 MiB block for MMIO windows (non-executable).
pub const FLAGS_BLOCK_DEVICE: u64 = DESC_VALID
    | DESC_AF
    | DESC_AP_RW_EL1
    | DESC_UXN
    | DESC_PXN
    | ATTR_DEVICE;

/// Normal (WB/WA cacheable) 4 KiB page, RW at EL1 **and EL0**, executable
/// by EL0 — the body of a server's private region (Phase 6).
pub const FLAGS_USER_RWX: u64 = DESC_VALID
    | DESC_PAGE
    | DESC_AF
    | DESC_SH_INNER
    | DESC_AP_RW_EL0
    | ATTR_NORMAL;

/// Device-nGnRnE 4 KiB page, RW at EL1 and EL0, non-executable — MMIO
/// windows granted to a trusted server (the display server's virtio-mmio).
pub const FLAGS_USER_DEVICE: u64 = DESC_VALID
    | DESC_PAGE
    | DESC_AF
    | DESC_AP_RW_EL0
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
/// If a covering L2 block descriptor already exists (the DDR pre-map), this
/// is a no-op — the range is already mapped.
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

    let l2e = (*l2).entry(l2_idx(vaddr));
    if l2e & DESC_VALID != 0 && l2e & DESC_TABLE == 0 {
        // A block descriptor already covers this address — nothing to do.
        return;
    }

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

/// Identity-map a region as a series of 2 MiB block descriptors.
///
/// `vaddr`/`paddr` must be 2 MiB-aligned and `size` a multiple of 2 MiB.
/// Used at boot to pre-map large ranges before the MMU is enabled.
///
/// # Safety
/// Frame allocator must be initialised.
pub unsafe fn map_block(vaddr: VirtAddr, paddr: PhysAddr, size: usize, flags: u64) {
    assert_eq!(vaddr & (2 * 1024 * 1024 - 1), 0, "map_block: vaddr unaligned");
    assert_eq!(paddr & (2 * 1024 * 1024 - 1), 0, "map_block: paddr unaligned");

    let l0 = core::ptr::addr_of_mut!(KERNEL_L0);
    let l1 = ensure_table(l0, l0_idx(vaddr));
    let l2 = ensure_table(l1, l1_idx(vaddr));

    let mut offset = 0;
    while offset < size {
        (*l2).set_entry(l2_idx(vaddr + offset), (paddr as u64 + offset as u64) | flags);
        offset += 2 * 1024 * 1024;
    }
}

// ── Per-task address spaces (Phase 6) ─────────────────────────────────────────

/// Allocate a page-table frame and reserve it in the frame allocator so it
/// is never handed out as normal memory while a task's tables are live.
///
/// # Safety
/// Frame allocator must be initialised.
unsafe fn alloc_table_frame() -> usize {
    let phys = alloc_frame().expect("page table: out of memory");
    super::frame::reserve_region(phys, PAGE_SIZE);
    phys
}

/// Recursively clone the kernel's table levels into freshly allocated
/// frames.  `src` is the physical address of a table page, `level` 0 = L0.
///
/// Every valid table descriptor is cloned; block/page descriptors (and
/// zero entries) are copied verbatim.
///
/// # Safety
/// Frame allocator must be initialised.  `src` must be a valid table.
unsafe fn clone_level(src: PhysAddr, level: usize) -> PhysAddr {
    let dst = alloc_table_frame() as *mut PageTable;
    let src_tbl = src as *const PageTable;
    for i in 0..512 {
        let e = (*src_tbl).entry(i);
        if e & DESC_VALID != 0 && e & DESC_TABLE != 0 && level < 3 {
            let child_phys = clone_level((e & 0x0000_FFFF_FFFF_F000) as usize, level + 1);
            (*dst).set_entry(i, (child_phys as u64) | DESC_VALID | DESC_TABLE);
        } else {
            (*dst).set_entry(i, e);
        }
    }
    dst as usize
}

/// Clone the kernel's identity map into a fresh set of tables and return
/// the physical address of the new L0 root.
///
/// The clone maps the entire DDR + MMIO EL1-only — exactly like the kernel
/// table — so the kernel can run (and touch any task's memory) while this
/// table is active.  EL0-visible pages are added afterwards with
/// `map_user_pages` / `set_user_block`.
///
/// # Safety
/// Frame allocator must be initialised.
pub unsafe fn clone_kernel_table() -> PhysAddr {
    let root = clone_level(kernel_l0_phys(), 0);
    log::debug!("page_table: cloned kernel table root={:#x}", root);
    root
}

/// Map the 4 KiB pages `[va .. va + size)` of the table rooted at `root`
/// with `flags` (typically `FLAGS_USER_*`), splitting any covering 2 MiB
/// block descriptor into an L3 table whose other entries stay EL1-only.
///
/// # Safety
/// `root` must be a valid task table (phys) and `va`/`size` page-aligned.
pub unsafe fn map_user_pages(root: PhysAddr, va: usize, size: usize, flags: u64) {
    debug_assert_eq!(va & (PAGE_SIZE - 1), 0, "map_user_pages: va unaligned");
    debug_assert_eq!(size & (PAGE_SIZE - 1), 0, "map_user_pages: size unaligned");

    let l0 = root as *mut PageTable;
    let l1 = ensure_table(l0, l0_idx(va));
    let l2 = ensure_table(l1, l1_idx(va));

    // Walk page-by-page so the split happens exactly once per block.
    let mut offset = 0;
    while offset < size {
        let cur = va + offset;
        let l2e = (*l2).entry(l2_idx(cur));
        if l2e & DESC_VALID != 0 && l2e & DESC_TABLE == 0 {
            // Covering block descriptor — split it into an L3 table.  The
            // other pages keep the block's attributes (normal vs device,
            // AP, shareability, XN) so we never silently change how the
            // kernel sees the rest of the 2 MiB range.
            let l3 = alloc_table_frame() as *mut PageTable;
            let block_base = cur & !(2 * 1024 * 1024 - 1);
            // Keep every descriptor bit the block carries (valid, AttrIndx,
            // NS, AP, SH, AF, nG, UXN, PXN) and switch the type bit to PAGE.
            let fill_flags = (l2e & (0xFFF | DESC_UXN | DESC_PXN)) | DESC_PAGE;
            for i in 0..512 {
                (*l3).set_entry(i, (block_base as u64 + (i as u64 * PAGE_SIZE as u64)) | fill_flags);
            }
            (*l2).set_entry(l2_idx(cur), (l3 as u64) | DESC_VALID | DESC_TABLE);
        }
        let l3 = ensure_table(l2, l2_idx(cur));
        (*l3).set_entry(l3_idx(cur), (cur as u64) | flags);
        offset += PAGE_SIZE;
    }
}

/// Patch the flags of a 2 MiB block descriptor in the table rooted at
/// `root` (used to grant an EL0 task a whole MMIO window in place, without
/// splitting it into pages).
///
/// # Safety
/// `root` must be a valid task table and `va` 2 MiB-aligned.
pub unsafe fn set_user_block(root: PhysAddr, va: usize, flags: u64) {
    debug_assert_eq!(va & (2 * 1024 * 1024 - 1), 0, "set_user_block: va unaligned");
    let l0 = root as *mut PageTable;
    let l1 = ensure_table(l0, l0_idx(va));
    let l2 = ensure_table(l1, l1_idx(va));
    let e = (*l2).entry(l2_idx(va));
    assert!(e & DESC_VALID != 0 && e & DESC_TABLE == 0, "set_user_block: no block");
    (*l2).set_entry(l2_idx(va), (e & 0x0000_FFFF_FFFF_F000) | flags);
}

/// Invalidate all stage-1 TLB entries (after any table/descriptor change).
///
/// # Safety
/// Must not be called from EL0.
pub fn flush_tlb() {
    unsafe {
        core::arch::asm!("tlbi vmalle1is", "dsb sy", "isb", options(nomem, nostack));
    }
}

/// Free every page-table frame reachable from `root` (the task's tables,
/// not the memory its descriptors point to), so the frames return to the
/// allocator.
///
/// Must be called when a task is killed *and its tables are no longer
/// active*.  The caller clears the task's `ttbr0` first.
///
/// # Safety
/// `root` must be a task table allocated by `clone_kernel_table` /
/// `alloc_table_frame`, not the kernel's own table.
pub unsafe fn free_task_tables(root: PhysAddr) {
    unsafe fn walk(tbl: PhysAddr, level: usize) {
        let t = tbl as *const PageTable;
        for i in 0..512 {
            let e = (*t).entry(i);
            if level < 3 && e & DESC_VALID != 0 && e & DESC_TABLE != 0 {
                walk((e & 0x0000_FFFF_FFFF_F000) as usize, level + 1);
            }
        }
        // Release the table frame itself (it was reserved by
        // `alloc_table_frame` — a plain free restores its bit).
        super::frame::free_frame(tbl);
    }
    debug_assert_ne!(root, kernel_l0_phys(), "free_task_tables: kernel table");
    walk(root, 0);
    log::debug!("page_table: freed task tables root={:#x}", root);
}

// ── MMU enable ────────────────────────────────────────────────────────────────

/// Build the kernel identity map and enable the MMU.
///
/// After this call all physical addresses equal their virtual addresses.
///
/// Mapping strategy: every range is pre-mapped *before* the MMU is enabled,
/// using 2 MiB block descriptors:
///
///   • 0x4000_0000 .. 0x5000_0000  — all 256 MiB of DDR (normal WB/WA, RWX)
///   • 0x0800_0000 .. 0x0A00_0000  — GICv3 distributor + redistributors
///   • 0x0900_0000 .. 0x0B00_0000  — PL011 UARTs
///   • 0x0A00_0000 .. 0x0C00_0000  — QEMU virtio-mmio transports
///       (32 slots × 0x20000, Phase 5: virtio-gpu + virtio-tablet)
///
/// Mapping the *entire* DDR means any frame later handed out by the frame
/// allocator (shared memory, guest RAM, page tables) is already mapped —
/// no table allocation is required after the MMU is live, which avoids the
/// classic "who maps the page-table pages?" chicken-and-egg problem.
///
/// # Safety
/// Must be called exactly once, after the frame allocator is initialised.
pub unsafe fn enable() {
    // 1. Entire 256 MiB DDR as 2 MiB blocks (identity, RWX for now).
    map_block(0x4000_0000, 0x4000_0000, 256 * 1024 * 1024, FLAGS_BLOCK_NORMAL);

    // 2. GICv3 distributor + redistributor (device-nGnRnE).
    map_block(0x0800_0000, 0x0800_0000, 2 * 1024 * 1024, FLAGS_BLOCK_DEVICE);

    // 3. PL011 UART (device-nGnRnE).
    map_block(0x0900_0000, 0x0900_0000, 2 * 1024 * 1024, FLAGS_BLOCK_DEVICE);

    // 4. QEMU virtio-mmio transports (32 slots, device-nGnRnE) — the
    //    Phase-5 display server drives virtio-gpu/virtio-tablet directly.
    map_block(0x0A00_0000, 0x0A00_0000, 2 * 1024 * 1024, FLAGS_BLOCK_DEVICE);

    // 5. Install TTBR0_EL1 (user/low address space) — we use TTBR0 for
    //    the identity map since our kernel VAs are below 0x0001_0000_0000.
    let ttbr0 = kernel_l0_phys() as u64;
    core::arch::asm!(
        "msr TTBR0_EL1, {t}",
        "isb",
        t = in(reg) ttbr0,
        options(nomem, nostack)
    );

    // 6. Flush TLB (all entries, all ASID).
    core::arch::asm!("tlbi vmalle1", "dsb sy", "isb", options(nomem, nostack));

    // 7. Enable MMU: set SCTLR_EL1.M (bit 0) and I-cache (bit 12).
    let mut sctlr: u64;
    core::arch::asm!("mrs {s}, SCTLR_EL1", s = out(reg) sctlr, options(nomem, nostack));
    sctlr |= 1 | (1 << 12); // M | I
    core::arch::asm!(
        "msr SCTLR_EL1, {s}",
        "isb",
        s = in(reg) sctlr,
        options(nomem, nostack)
    );

    log::info!("MMU enabled — identity map active (DDR + MMIO pre-mapped)");
}
