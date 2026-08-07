//! Global heap allocator for servers (feature `alloc`).
//!
//! A server binary is a freestanding image with no allocator; the Phase-5
//! UI toolkit needs `Vec`/`Box`/`format!`.  This is a classic first-fit
//! free-list allocator whose backing memory comes from the kernel's frame
//! allocator (`sys::alloc_frames`) — the Minix-style way of getting memory:
//! ask the kernel, carve it up in user space.
//!
//! Blocks are 16-byte headers + payload (16-byte aligned), so layouts with
//! alignment up to 16 are supported.  Freeing coalesces with the next block.
//! The heap grows in 16-frame (64 KiB) chunks on demand, up to a 1 MiB cap.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::sys;

/// Size of a block header (fits a `size` + free-list `next` pointer).
const HEADER: usize = 16;
/// Minimum block size (header + 16-byte aligned payload).
const BLOCK: usize = 32;

const PAGE: usize = 4096;
const INITIAL_PAGES: usize = 16; // 64 KiB
const GROW_PAGES: usize = 16;
const MAX_PAGES: usize = 256; // 1 MiB heap cap

/// A heap block.  `next` is only meaningful while the block is free.
#[repr(C)]
struct Block {
    size: usize,
    next: *mut Block,
}

impl Block {
    #[inline]
    unsafe fn payload(&self) -> *mut u8 {
        (self as *const Block as usize + HEADER) as *mut u8
    }
}

/// First-fit free list over kernel frames.
pub struct Heap {
    head: *mut Block,
    pages: usize,
}

impl Heap {
    pub const fn new() -> Self {
        Self { head: ptr::null_mut(), pages: 0 }
    }

    /// Lazy init: claim the first chunk of frames from the kernel.
    fn ensure(&mut self) -> bool {
        if !self.head.is_null() {
            return true;
        }
        let base = sys::alloc_frames(INITIAL_PAGES as u32) as usize;
        if base == 0 {
            return false;
        }
        self.pages = INITIAL_PAGES;
        let block = base as *mut Block;
        unsafe {
            (*block).size = INITIAL_PAGES * PAGE;
            (*block).next = ptr::null_mut();
        }
        self.head = block;
        true
    }

    /// Claim another chunk of frames and prepend it to the free list.
    fn grow(&mut self) -> bool {
        if self.pages >= MAX_PAGES {
            return false;
        }
        let n = (MAX_PAGES - self.pages).min(GROW_PAGES);
        let base = sys::alloc_frames(n as u32) as usize;
        if base == 0 {
            return false;
        }
        self.pages += n;
        let block = base as *mut Block;
        unsafe {
            (*block).size = n * PAGE;
            (*block).next = self.head;
        }
        self.head = block;
        true
    }

    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        if !self.ensure() {
            return ptr::null_mut();
        }
        let need = block_size(layout);
        loop {
            let mut prev: *mut Block = ptr::null_mut();
            let mut cur = self.head;
            while !cur.is_null() {
                let size = (*cur).size;
                if size >= need {
                    // Split off a new block if the remainder is usable.
                    if size - need >= BLOCK {
                        let rest = (cur as usize + need) as *mut Block;
                        (*rest).size = size - need;
                        (*rest).next = (*cur).next;
                        (*cur).size = need;
                        (*cur).next = ptr::null_mut();
                        if prev.is_null() {
                            self.head = rest;
                        } else {
                            (*prev).next = rest;
                        }
                    } else if prev.is_null() {
                        self.head = (*cur).next;
                    } else {
                        (*prev).next = (*cur).next;
                    }
                    (*cur).next = ptr::null_mut(); // allocated: not on list
                    return (*cur).payload();
                }
                prev = cur;
                cur = (*cur).next;
            }
            // No fit — grow the heap once and retry.
            if !self.grow() {
                return ptr::null_mut();
            }
        }
    }

    unsafe fn free(&mut self, ptr: *mut u8) {
        let block = (ptr as usize - HEADER) as *mut Block;
        if self.head.is_null() || (block as usize) < (self.head as usize) {
            (*block).next = self.head;
            self.head = block;
        } else {
            // Coalesce with the following block; insert sorted by address.
            let mut prev = self.head;
            let mut cur = (*prev).next;
            while !cur.is_null() && (cur as usize) < (block as usize) {
                prev = cur;
                cur = (*cur).next;
            }
            (*block).next = cur;
            (*prev).next = block;
            if !cur.is_null() && (block as usize) + (*block).size == (cur as usize) {
                // Coalesce with next.
                (*block).size += (*cur).size;
                (*block).next = (*cur).next;
            }
        }
    }
}

/// Round a layout up to a whole block: header + 16-aligned payload.
fn block_size(layout: Layout) -> usize {
    let size = layout.size();
    let align = layout.align().max(HEADER);
    let mut total = HEADER + size;
    if align > HEADER {
        total += align - HEADER;
    }
    total.next_multiple_of(HEADER)
}

static mut HEAP: Heap = Heap::new();

/// The global allocator installed for server binaries.
pub struct TanixAlloc;

unsafe impl GlobalAlloc for TanixAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        (*core::ptr::addr_of_mut!(HEAP)).alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        (*core::ptr::addr_of_mut!(HEAP)).free(ptr)
    }
}

#[global_allocator]
static ALLOC: TanixAlloc = TanixAlloc;
