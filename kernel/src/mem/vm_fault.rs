#![allow(dead_code)]
//! Phase 19: user-fault resolution — demand paging, copy-on-write and
//! lazy stack growth.
//!
//! EL0 data aborts that reach `exception.rs` are offered to
//! `resolve_user_fault` first.  The resolver may repair the faulting
//! translation (map a page) and let the instruction re-execute; anything
//! it declines falls through to the existing kill path.
//!
//! Three kinds of fault are repaired:
//!
//!   • translation fault inside a `REGION_STACK` window above its base —
//!     the covering page is identity-mapped RW (the rest of the window is
//!     faulted in as the stack grows down);
//!   • translation fault inside a `REGION_DEMAND` window — the page
//!     aliases the shared zero page through a COW descriptor, so a read
//!     needs no frame while a write triggers the split below;
//!   • permission / access-flag fault on a COW-tagged page — the frame is
//!     copied into a fresh private frame and the page is remapped RW
//!     (the copy-on-write split; the other aliases keep the shared frame).
//!
//! The stack-growth and demand windows are described by the current task's
//! region table (`sched::task::regions`); a COW page is recognised from
//! the descriptor itself (`DESC_COW_TAG`), so no per-page bookkeeping is
//! needed for shared frames.
//!
//! Every mutation runs under `SCHED_LOCK` (the faulting task is the
//! current one, its TCB is stable, and the frame allocator's ordering
//! `FRAME_LOCK` ⊂ `SCHED_LOCK` is preserved).

use super::{frame, page_table, PAGE_SIZE};
use crate::sched::task::{current_regions, sched_lock, REGION_DEMAND, REGION_STACK};

/// DFSC class bases (bits [5:0] of ESR_EL1), stage-1 only: each class
/// covers the four levels (L0..L3).
const DFSC_TT_FIRST: u64 = 0b000100; // translation fault
const DFSC_AF_FIRST: u64 = 0b001100; // access-flag fault
const DFSC_PERM_FIRST: u64 = 0b010100; // permission fault

fn in_class(dfsc: u64, first: u64) -> bool {
    dfsc >= first && dfsc <= first + 3
}

/// Decide whether the fault at `far` (ESR_EL1 = `esr`, DABT from the
/// current EL0 task) can be repaired.  Returns true when the faulting
/// instruction may re-execute; false when the fault is irreparable (the
/// caller kills the task).
///
/// Holds `SCHED_LOCK` for the whole decision + mutation.
pub fn resolve_user_fault(far: usize, esr: u64) -> bool {
    let d = esr & 0x3F;
    let page = far & !(PAGE_SIZE - 1);
    let task_name = crate::sched::task::current_name();

    let lock = sched_lock();
    lock.lock();

    let ttbr0 = crate::sched::task::current_ttbr0() as usize;
    if ttbr0 == 0 {
        lock.unlock();
        return false; // kernel context faults never repaired
    }
    let regions = current_regions();

    let mut handled = false;
    if in_class(d, DFSC_TT_FIRST) {
        // Translation fault: find the covering region and repair it.
        for r in regions.iter() {
            if r.kind == REGION_STACK
                && page >= r.base
                && page < r.base + r.pages * PAGE_SIZE
            {
                unsafe {
                    page_table::map_user_frame(ttbr0, page, page, page_table::FLAGS_USER_RWX);
                }
                log::debug!("phase 19: stack grow -> {:#x} (task '{}')", page, task_name);
                handled = true;
                break;
            }
            if r.kind == REGION_DEMAND
                && page >= r.base
                && page < r.base + r.pages * PAGE_SIZE
            {
                // Alias the shared zero page COW; the follow-up write of a
                // zero-fill pop splits it into a private frame.
                unsafe {
                    page_table::map_user_frame(
                        ttbr0,
                        page,
                        page_table::zero_page_phys(),
                        page_table::FLAGS_USER_COW,
                    );
                }
                log::debug!(
                    "phase 19: demand page {:#x} -> shared zero page (task '{}')",
                    page,
                    task_name
                );
                handled = true;
                break;
            }
        }
    } else if in_class(d, DFSC_PERM_FIRST) {
        // Permission fault: only COW-tagged pages are repaired (split).
        let entry = unsafe { page_table::read_user_page(ttbr0, page) };
        log::debug!(
            "phase 19: PERM fault {:#x} entry={:#x} (task '{}')",
            page,
            entry,
            task_name
        );
        handled = if entry & page_table::DESC_VALID != 0 && entry & page_table::DESC_COW_TAG != 0 {
            unsafe { cow_split(ttbr0, page, entry) }
        } else {
            false
        };
    } else if in_class(d, DFSC_AF_FIRST) {
        // Access-flag fault: COW-tagged pages split (the first write to a
        // zero-fill alias can surface as an AF fault instead of a
        // permission fault); plain pages just get AF set in place.
        let entry = unsafe { page_table::read_user_page(ttbr0, page) };
        log::debug!(
            "phase 19: AF fault {:#x} entry={:#x} (task '{}')",
            page,
            entry,
            task_name
        );
        handled = if entry & page_table::DESC_VALID != 0 && entry & page_table::DESC_COW_TAG != 0 {
            unsafe { cow_split(ttbr0, page, entry) }
        } else if entry & page_table::DESC_VALID != 0
            && entry & page_table::DESC_COW_TAG == 0
            && entry & page_table::DESC_AF == 0
        {
            unsafe { page_table::write_user_page(ttbr0, page, entry | page_table::DESC_AF) };
            log::debug!("phase 19: AF fixup on {:#x} (task '{}')", page, task_name);
            true
        } else {
            false
        };
    }

    lock.unlock();
    handled
}

/// Copy-on-write split: the faulting task's page becomes a private RW
/// frame holding a copy of the shared frame's contents (caller holds
/// `SCHED_LOCK`).
unsafe fn cow_split(root: usize, va: usize, entry: u64) -> bool {
    let shared = (entry & 0x0000_FFFF_FFFF_F000) as usize;
    let fresh = match frame::alloc_frame() {
        Some(f) => f,
        None => return false, // OOM — let the task die
    };
    core::ptr::copy_nonoverlapping(shared as *const u8, fresh as *mut u8, PAGE_SIZE);
    page_table::write_user_page(root, va, (fresh as u64) | page_table::FLAGS_USER_RWX);
    log::info!(
        "phase 19: COW split {:#x} ({:#x} -> {:#x}, private RW) — task '{}'",
        va,
        shared,
        fresh,
        crate::sched::task::current_name()
    );
    true
}