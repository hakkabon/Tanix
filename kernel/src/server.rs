//! Server-process registry and spawn (Phase 4).
//!
//! Each server is a separate no_std binary (a member of the workspace under
//! `servers/`), embedded into the kernel at compile time via
//! `include_bytes!` (feature `embed-servers`), exactly like the Zephyr-stub
//! guest.  At spawn time the kernel:
//!
//!   1. zeroes the server's private RAM region,
//!   2. loads the binary (ELF, linked at its final address),
//!   3. creates a scheduler task whose stack sits at the top of the region,
//!   4. hands the task a `BootInfo` block (syscall table + own task id)
//!      preloaded into its callee-saved x19.
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

use crate::ipc::syscall::table_ptr;
use crate::mem::PAGE_SIZE;
use crate::sched::task::spawn_server;
use crate::sched::{BootInfo, TaskId};
use crate::vm::loader;

/// Private memory region size per server (frames).  128 KiB each.
pub const SERVER_RAM_PAGES: usize = 32;

// ── Embedded binaries ─────────────────────────────────────────────────────────

#[cfg(feature = "embed-servers")]
static SERVER_BINS: &[(&str, &[u8])] = &[
    ("init", include_bytes!(env!("TANIX_INIT_BIN_PATH"))),
    ("pm", include_bytes!(env!("TANIX_PM_BIN_PATH"))),
    ("mem", include_bytes!(env!("TANIX_MEM_BIN_PATH"))),
    ("dev", include_bytes!(env!("TANIX_DEV_BIN_PATH"))),
    ("worker", include_bytes!(env!("TANIX_WORKER_BIN_PATH"))),
];

#[cfg(not(feature = "embed-servers"))]
static SERVER_BINS: &[(&str, &[u8])] = &[];

/// Fixed link/runtime base per server — MUST match `--defsym=LINK_BASE` in
/// each crate's `servers/*/build.rs`.
///
/// The kernel image ends around 0x40530000 and the Phase-3 guest occupies
/// 0x4052f000..0x4062f000, so the first free 128 KiB-aligned slot is
/// 0x40700000 (256 MiB DDR window starts at 0x40000000).
pub const SERVER_BASES: &[(&str, usize)] = &[
    ("init",   0x4070_0000),
    ("pm",     0x4072_0000),
    ("mem",    0x4074_0000),
    ("dev",    0x4076_0000),
    ("worker", 0x4078_0000),
];

// ── Spawn ─────────────────────────────────────────────────────────────────────

/// Spawn a server by its registered name.  Returns the new task id.
///
/// This is what the `spawn` syscall invokes; the kernel's own Phase-4 boot
/// calls it for `init` directly.
pub fn spawn_by_name(name: &str) -> Result<TaskId, i32> {
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
    let entry = loader::load_flat(bin, 0, base + ram_size).map_err(|_| -9)?;

    // Task with its stack at the top of the region.
    let stack_top = base + ram_size;
    let boot = BootInfo {
        syscalls: table_ptr(),
        task_id: 0, // filled by the scheduler with the real id
    };
    let id = spawn_server(name, entry, stack_top, boot).ok_or(-8)?;

    log::info!(
        "server: spawned '{}' {:?} region={:#x}+{} KB entry={:#x}",
        name, id, base, ram_size / 1024, entry
    );
    Ok(id)
}

/// Whether any server binaries are embedded (feature enabled + built).
pub fn available() -> bool {
    !SERVER_BINS.is_empty()
}
