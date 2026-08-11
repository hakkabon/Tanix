//! Server-process registry and spawn (Phase 6/7).
//!
//! Each server is a separate no_std binary (a member of the workspace under
//! `servers/`), embedded into the kernel at compile time via
//! `include_bytes!` (feature `embed-servers`), exactly like the Zephyr-stub
//! guest.  At spawn time the kernel:
//!
//!   1. zeroes the server's private RAM region,
//!   2. loads the binary (ELF, linked at its final address),
//!   3. **clones the kernel's page tables** into a per-task address space
//!      (Phase 6): every EL1-only mapping is kept, and the server's own
//!      region pages (plus, for the display server, its virtio-mmio window)
//!      are made EL0-visible,
//!   4. creates an **EL0** scheduler task (SPSR_EL1 = EL0t) with a user
//!      stack, a kernel stack at the top of its region, and a `BootInfo`
//!      block in its region (task id) preloaded into its callee-saved x19,
//!   5. hands it the task via `spawn_server_user`; from then on the server
//!      calls the kernel through `svc #0` (see `ipc::syscall`).
//!
//! Phase 7 assigns each server a scheduling priority (see `SERVER_PRIOS`):
//! the display server (GPU owner) is highest, the `hog` spin-demo is
//! lowest, the Phase-4 services sit in the middle.
//!
//! Unlike the Phase-3 guest (loaded at a dynamic address), the servers link
//! at a **fixed** physical address (`LINK_BASE` in each crate's build.rs).
//! The addresses are absolute in the image (function pointers in vtables,
//! static data), so relocation would be required to run them anywhere else;
//! the fixed bases below make them plain statically-linked executables,
//! exactly like the kernel itself.  The bases are chosen clear of the
//! kernel image and the Phase-3 guest RAM, and are identity-mapped by the
//! MMU.
//!
//! `init` is the root server; `pm` (process manager), `mem` (memory
//! service) and `dev` (device service) are started by `init` through the
//! `spawn` syscall, and a `worker` binary exists so `pm` has something
//! realistic to exec.

use crate::mem::{page_table, PAGE_SIZE};
use crate::sched::task::spawn_server_user_locked;
use crate::sched::{BootInfo, TaskId};
use crate::vm::loader;

/// Private memory region size per server (frames).  128 KiB each.
pub const SERVER_RAM_PAGES: usize = 32;

/// User stack size per server (below the EL1-only kernel stack at the top
/// of the region).
pub const USER_STACK_SIZE: usize = 16 * 1024;

/// virtio-mmio window granted to the display server: 32 transports × 0x200
/// bytes, all within 16 KiB (see `servers/display/src/virtio.rs`).
const DISPLAY_MMIO_BASE: usize = 0x0A00_0000;
const DISPLAY_MMIO_SIZE: usize = 32 * 0x200;

/// PL011 UART0 page granted to the `dev` server (its `M_DEV_WRITE` service
/// writes the console directly).
const UART0_BASE: usize = 0x0900_0000;

// ── Embedded binaries ─────────────────────────────────────────────────────────

#[cfg(feature = "embed-servers")]
static SERVER_BINS: &[(&str, &[u8])] = &[
    ("init", include_bytes!(env!("TANIX_INIT_BIN_PATH"))),
    ("pm", include_bytes!(env!("TANIX_PM_BIN_PATH"))),
    ("mem", include_bytes!(env!("TANIX_MEM_BIN_PATH"))),
    ("dev", include_bytes!(env!("TANIX_DEV_BIN_PATH"))),
    ("worker", include_bytes!(env!("TANIX_WORKER_BIN_PATH"))),
    ("display", include_bytes!(env!("TANIX_DISPLAY_BIN_PATH"))),
    ("ui-demo", include_bytes!(env!("TANIX_UI_DEMO_BIN_PATH"))),
    ("wm", include_bytes!(env!("TANIX_WM_BIN_PATH"))),
    ("counter", include_bytes!(env!("TANIX_COUNTER_BIN_PATH"))),
    ("clock", include_bytes!(env!("TANIX_CLOCK_BIN_PATH"))),
    ("hog", include_bytes!(env!("TANIX_HOG_BIN_PATH"))),
    ("ramfs", include_bytes!(env!("TANIX_RAMFS_BIN_PATH"))),
    ("shell", include_bytes!(env!("TANIX_SHELL_BIN_PATH"))),
    ("net", include_bytes!(env!("TANIX_NET_BIN_PATH"))),
    ("ping", include_bytes!(env!("TANIX_PING_BIN_PATH"))),
    ("pong", include_bytes!(env!("TANIX_PONG_BIN_PATH"))),
];

#[cfg(not(feature = "embed-servers"))]
static SERVER_BINS: &[(&str, &[u8])] = &[];

/// Fixed link/runtime base per server — MUST match `--defsym=LINK_BASE` in
/// each crate's `servers/*/build.rs`.
///
/// The bases must stay clear of everything the frame allocator hands out:
/// the kernel image (which embeds the server binaries; in a debug build the
/// ~0.9 MB-per-binary set pushes the image end to ~0x409B_0000) and the
/// Phase-3 guest RAM / shmem and the display framebuffer (4 MiB) allocated
/// just after it.  The 0x4100_0000 block (13 × 128 KiB slots = 1.625 MiB)
/// leaves ~6 MiB of headroom above the Phase-3 + framebuffer allocations in
/// a full debug build.
///
/// `reserve_regions()` runs *before* any dynamic allocation (kmain, right
/// after the frame allocator starts), so neither the guest nor any server
/// can receive frames that overlap a live server image.
pub const SERVER_BASES: &[(&str, usize)] = &[
    ("init",    0x4100_0000),
    ("pm",      0x4102_0000),
    ("mem",     0x4104_0000),
    ("dev",     0x4106_0000),
    ("worker",  0x4108_0000),
    ("display", 0x410A_0000),
    ("ui-demo", 0x410C_0000),
    ("hog",     0x410E_0000),
    ("wm",      0x4110_0000),
    ("counter", 0x4112_0000),
    ("clock",   0x4114_0000),
    ("ramfs",   0x4116_0000),
    ("shell",   0x4118_0000),
    ("net",     0x411A_0000),
    ("ping",    0x411C_0000),
    ("pong",    0x411E_0000),
];

/// Scheduling priority per server (Phase 7) — lower runs first.  The
/// display server owns the GPU, so it is highest; the `hog` demo spins on
/// the CPU at the lowest priority and is only scheduled when everything
/// else is idle.
pub const SERVER_PRIOS: &[(&str, u8)] = &[
    ("init",    128),
    ("pm",      128),
    ("mem",     128),
    ("dev",     128),
    ("worker",  128),
    ("display",  32),
    ("wm",       48),
    ("ramfs",    64),
    ("ui-demo",  96),
    ("counter",  96),
    ("clock",    96),
    ("shell",    96),
    ("net",      96),
    ("ping",     96),
    ("pong",     96),
    ("hog",     192),
];

// ── Spawn ─────────────────────────────────────────────────────────────────────

/// Spawn a server by its registered name.  Returns the new task id.
///
/// This is what the kernel's own Phase-4/5/8/9/10 boot paths call (not
/// under `SCHED_LOCK`); the `spawn` / `exec` syscalls use the locked
/// variant below instead — the syscall dispatcher already holds the lock.
pub fn spawn_by_name(name: &str) -> Result<TaskId, i32> {
    let lock = crate::sched::task::sched_lock();
    lock.lock();
    let r = spawn_by_name_locked(name);
    lock.unlock();
    r
}

/// `spawn_by_name` with `SCHED_LOCK` already held (Phase 11).
pub fn spawn_by_name_locked(name: &str) -> Result<TaskId, i32> {
    let bin = SERVER_BINS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
        .ok_or(-7)?; // unknown server name
    let base = SERVER_BASES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
        .ok_or(-7)?; // unknown server name

    let ram_size = SERVER_RAM_PAGES * PAGE_SIZE;

    // Zero the region, then load the binary.  The image links at its final
    // address, so the loader runs with base 0 (dest = vaddr = fixed phys);
    // the region bound is `base + ram_size`.
    unsafe {
        core::ptr::write_bytes(base as *mut u8, 0, ram_size);
    }
    let img = loader::load_flat_full(bin, 0, base + ram_size).map_err(|_| -9)?;
    let entry = img.entry;
    let image_end = img.image_end; // absolute (servers link at LINK_BASE)

    // ── Phase 6: per-task address space ─────────────────────────────────────
    // Clone the kernel's identity map (all of it EL1-only) and open up
    // exactly what this server may touch at EL0:
    //   • its own region: image + bootinfo page + user stack (RWX for now;
    //     RO-for-text is a later tightening),
    //   • display only: the virtio-mmio transport window.
    // The kernel stack at the top of the region stays EL1-only.
    let ttbr0 = unsafe { page_table::clone_kernel_table() };

    let boot_page = image_end;
    let user_area_end = boot_page + PAGE_SIZE + USER_STACK_SIZE;
    assert!(
        user_area_end <= base + ram_size,
        "server '{}': image too large for its region ({:#x} .. {:#x})",
        name, base, base + ram_size
    );
    unsafe {
        page_table::map_user_pages(ttbr0, base, boot_page - base, page_table::FLAGS_USER_RWX);
        page_table::map_user_pages(ttbr0, boot_page, PAGE_SIZE, page_table::FLAGS_USER_RWX);
        page_table::map_user_pages(
            ttbr0,
            boot_page + PAGE_SIZE,
            USER_STACK_SIZE,
            page_table::FLAGS_USER_RWX,
        );
        if name == "display" {
            page_table::map_user_pages(
                ttbr0,
                DISPLAY_MMIO_BASE,
                DISPLAY_MMIO_SIZE,
                page_table::FLAGS_USER_DEVICE,
            );
        }
        if name == "dev" {
            page_table::map_user_pages(ttbr0, UART0_BASE, PAGE_SIZE, page_table::FLAGS_USER_DEVICE);
        }
    }

    let kernel_stack_top = base + ram_size;
    let sp_el0 = boot_page + PAGE_SIZE + USER_STACK_SIZE;
    let boot = BootInfo { task_id: 0 };
    let id = unsafe {
        spawn_server_user_locked(
            name,
            entry,
            ttbr0 as u64,
            sp_el0,
            kernel_stack_top,
            boot_page,
            boot,
        )
    }
    .ok_or(-8)?;
    // Phase 7: assign the server's scheduling priority.
    let prio = SERVER_PRIOS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| *p)
        .unwrap_or(crate::sched::PRIO_NORMAL);
    unsafe {
        crate::sched::task::set_task_priority_locked(id, prio);
    }

    log::info!(
        "server: spawned '{}' {:?} EL0 region={:#x}+{} KB entry={:#x} image_end={:#x} ttbr0={:#x} sp_el0={:#x} prio={}",
        name, id, base, ram_size / 1024, entry, image_end, ttbr0, sp_el0, prio
    );
    Ok(id)
}

/// Whether any server binaries are embedded (feature enabled + built).
pub fn available() -> bool {
    !SERVER_BINS.is_empty()
}

/// Reserve every server's private RAM region in the physical frame
/// allocator.
///
/// The frame allocator only reserves `[RAM_START .. kernel_end]` at boot;
/// without this, kernel services (e.g. the display framebuffer, handed out
/// as one large contiguous run) can be allocated frames that overlap a
/// live server image, silently corrupting its code and data.  Must be
/// called before any server task can run or make allocations.
pub fn reserve_regions() {
    for &(_, base) in SERVER_BASES {
        let size = SERVER_RAM_PAGES * PAGE_SIZE;
        unsafe {
            crate::mem::frame::reserve_region(base, size);
        }
        log::info!("server: reserved region {:#x}+{} KiB", base, size / 1024);
    }
}
