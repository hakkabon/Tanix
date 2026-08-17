//! Memory management subsystem.
//!
//! Provides:
//!   • `PhysAddr` / `VirtAddr` type aliases.
//!   • `frame`       — physical frame allocator (bitmap, 256 MiB DDR).
//!   • `page_table`  — AArch64 4-level page tables and MMU enable.
//!   • `vm_fault`    — Phase 19 fault resolution (demand paging, COW,
//!                     stack growth).

pub mod frame;
pub mod page_table;
pub mod vm_fault;

/// Physical address type alias — makes intent clearer than a raw `usize`.
pub type PhysAddr = usize;

/// Virtual address type alias.
pub type VirtAddr = usize;

/// Page size (4 KiB).
pub const PAGE_SIZE: usize = 4096;
