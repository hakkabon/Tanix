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
/// `virt` only — `sbsa-ref` has no virtio-mmio (Phase 16).
const DISPLAY_MMIO_BASE: usize = 0x0A00_0000;
const DISPLAY_MMIO_SIZE: usize = 32 * 0x200;

// ── MMIO capability table (Phase 19) ─────────────────────────────────────────
//
// A server may map a device window only when the window is covered by one
// of its granted capabilities — `SYS_MAP_DEVICE` and `SYS_MAP_CAP` are both
// validated against this table.  The windows are *permissions*, resolved to
// machine-specific addresses at check time; the mapping itself happens on
// demand through the syscalls.

/// What a capability names (the address depends on the machine).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapKind {
    /// The machine's PL011 UART window.
    Uart,
    /// QEMU `virt` virtio-mmio transport window (none on `sbsa-ref`).
    VirtioMmio,
    /// PCIe ECAM config space.
    Ecam,
    /// PCI memory BAR window.
    PciMem,
}

/// One granted capability: a device family and a window size.
#[derive(Clone, Copy)]
pub struct MmioCap {
    pub kind: CapKind,
    pub size: usize,
}

/// Per-server capability grants (Phase 19).  `dev` owns the console UART,
/// `net` the PCIe config + BAR space, `display` the virtio-mmio transports.
pub const SERVER_MMIO_CAPS: &[(&str, &[MmioCap])] = &[
    ("dev", &[MmioCap { kind: CapKind::Uart, size: PAGE_SIZE }]),
    (
        "net",
        &[
            MmioCap { kind: CapKind::Ecam, size: 16 * 1024 * 1024 },
            MmioCap { kind: CapKind::PciMem, size: 0x3000_0000 },
        ],
    ),
    // Phase 20: `fs` owns the virtio-blk PCI device (same windows as `net`).
    (
        "fs",
        &[
            MmioCap { kind: CapKind::Ecam, size: 16 * 1024 * 1024 },
            MmioCap { kind: CapKind::PciMem, size: 0x3000_0000 },
        ],
    ),
    ("display", &[MmioCap { kind: CapKind::VirtioMmio, size: DISPLAY_MMIO_SIZE }]),
];

/// Resolve a capability window to machine-specific `(base, size)`.
/// `None` when the cap index is out of range or the window does not exist
/// on this machine (e.g. virtio-mmio on `sbsa-ref`).
pub fn cap_window_for(name: &str, idx: usize) -> Option<(usize, usize)> {
    let caps = SERVER_MMIO_CAPS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)?;
    let cap = caps.get(idx)?;
    let m = crate::arch::aarch64::machine();
    let base = match cap.kind {
        CapKind::Uart => m.uart_base,
        CapKind::VirtioMmio => m.virtio_mmio_base,
        CapKind::Ecam => {
            if m.id == crate::arch::aarch64::machine::MACHINE_SBSA_REF {
                0xF000_0000
            } else {
                0x3F00_0000
            }
        }
        CapKind::PciMem => {
            if m.id == crate::arch::aarch64::machine::MACHINE_SBSA_REF {
                0xC000_0000
            } else {
                0x1000_0000
            }
        }
    };
    if base == 0 {
        return None;
    }
    Some((base, cap.size))
}

/// Does the caller's capability set permit mapping `[base, base + size)`?
pub fn cap_permits(name: &str, base: usize, size: usize) -> bool {
    (0..16).any(|i| match cap_window_for(name, i) {
        Some((cap_base, cap_size)) => base >= cap_base && size <= cap_size && base - cap_base + size <= cap_size,
        None => false,
    })
}

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
    ("sec", include_bytes!(env!("TANIX_SEC_BIN_PATH"))),
    ("fs", include_bytes!(env!("TANIX_FS_BIN_PATH"))),
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
    ("sec",     0x4120_0000),
    ("fs",      0x4122_0000),
];

/// Phase 16: the fixed link bases above are chosen for the `virt` machine's
/// 1 GiB DRAM window (`0x4000_0000`).  Other machines move the whole RAM
/// window by a constant offset — `sbsa-ref` starts its DDR at 1 TiB
/// (`0x100_0000_0000`) — and every server binary is *linked at the shifted
/// address* (the sbsa server build sets `TANIX_LINK_SHIFT` in `justfile`,
/// which all `servers/*/build.rs` add to their base).  This must stay in
/// lockstep with those build scripts.
fn machine_base_shift() -> usize {
    let m = crate::arch::aarch64::machine();
    if m.id == crate::arch::aarch64::machine::MACHINE_SBSA_REF {
        // The sbsa-ref kernel image — .text/.rodata/.data, .bss (ends
        // 0x100010f0158), the EL3 stack and the secure payload — spans
        // `_start`..`__kernel_end` (0x10000080000..0x10001110000) and
        // overlaps the plain-shifted server map (worker..wm).  Walk the
        // whole map up by 32 MiB so every server region sits above the
        // kernel image; mirrors `TANIX_LINK_SHIFT=0xFFC2000000` in the
        // `servers-sbsa` recipe of `justfile`.
        m.dram_base.wrapping_sub(0x4000_0000) + 0x200_0000
    } else {
        0
    }
}

/// The runtime base of server `name`, machine-aware (Phase 16).
pub fn server_link_base(name: &str) -> usize {
    let virt_base = SERVER_BASES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b);
    match virt_base {
        Some(b) => b + machine_base_shift(),
        None => {
            log::error!("server: unknown server name '{}'", name);
            0
        }
    }
}

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
    ("sec",      96),
    ("fs",       96),
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
    let base = server_link_base(name);
    if base == 0 {
        return Err(-7);
    }

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
    //   • its own region: image + bootinfo page (RWX for now; RO-for-text
    //     is a later tightening),
    //   • the top 4 KiB of its user stack — the rest of the stack window is
    //     faulted in page-by-page as it grows down (Phase 19),
    //   • Phase 19: device windows are no longer granted here — servers
    //     request them through SYS_MAP_DEVICE / SYS_MAP_CAP, which the
    //     kernel validates against SERVER_MMIO_CAPS.
    // The kernel stack at the top of the region stays EL1-only.
    let ttbr0 = unsafe { page_table::clone_kernel_table() };

    let boot_page = image_end;
    let user_area_end = boot_page + PAGE_SIZE + USER_STACK_SIZE;
    assert!(
        user_area_end <= base + ram_size,
        "server '{}': image too large for its region ({:#x} .. {:#x})",
        name, base, base + ram_size
    );
    let stack_window_base = boot_page + PAGE_SIZE;
    unsafe {
        page_table::map_user_pages(ttbr0, base, boot_page - base, page_table::FLAGS_USER_RWX);
        page_table::map_user_pages(ttbr0, boot_page, PAGE_SIZE, page_table::FLAGS_USER_RWX);
        page_table::map_user_pages(
            ttbr0,
            stack_window_base + USER_STACK_SIZE - PAGE_SIZE,
            PAGE_SIZE,
            page_table::FLAGS_USER_RWX,
        );
        // `display` (legacy, `virt` only) probes the virtio-mmio window with
        // fixed build-time addresses and never asks for it — keep the
        // spawn-time grant (its grant is mirrored in SERVER_MMIO_CAPS).  On
        // `sbsa-ref` the window does not exist.  Phase 19: all other windows
        // are requested through SYS_MAP_DEVICE / SYS_MAP_CAP instead.
        if name == "display" && DISPLAY_MMIO_BASE != 0 {
            page_table::map_user_pages(
                ttbr0,
                DISPLAY_MMIO_BASE,
                DISPLAY_MMIO_SIZE,
                page_table::FLAGS_USER_DEVICE,
            );
        }
    }

    let kernel_stack_top = base + ram_size;
    let sp_el0 = boot_page + PAGE_SIZE + USER_STACK_SIZE;
    let boot = BootInfo { task_id: 0, machine: crate::arch::aarch64::machine::machine().id };
    let id = unsafe {
        spawn_server_user_locked(
            name,
            entry,
            ttbr0 as u64,
            sp_el0,
            kernel_stack_top,
            boot_page,
            boot,
            base,
            image_end,
        )
    }
    .ok_or(-8)?;
    // Phase 19: record the (bottom, top] stack window so the fault
    // resolver can grow it page-by-page from the single mapped top page.
    if !crate::sched::task::push_region_for(
        id,
        crate::sched::task::REGION_STACK,
        stack_window_base,
        USER_STACK_SIZE / PAGE_SIZE,
    ) {
        log::warn!("server: '{}' stack region registration failed", name);
    }
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
    for &(name, _) in SERVER_BASES {
        let base = server_link_base(name);
        let size = SERVER_RAM_PAGES * PAGE_SIZE;
        unsafe {
            crate::mem::frame::reserve_region(base, size);
        }
        log::info!("server: reserved region {:#x}+{} KiB", base, size / 1024);
    }
}
