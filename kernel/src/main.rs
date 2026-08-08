#![no_std]
#![no_main]

mod arch;
mod hypervisor;
mod ipc;
mod irq;
mod mem;
mod panic;
mod sched;
mod server;
mod virtio;
mod vm;

// ── Guest VM binary ───────────────────────────────────────────────────────────

/// Phase 3 Zephyr-stub with VirtIO driver, embedded at compile time.
/// Build with `just zephyr-stub` before `just kernel-embed`.
///
/// The path is emitted by `build.rs` and is profile-aware (debug/release).
#[cfg(feature = "embed-zephyr-stub")]
static ZEPHYR_STUB_BIN: &[u8] = include_bytes!(env!("TANIX_STUB_BIN_PATH"));

/// Fallback: an infinite yield loop — boots, hands control back to the
/// kernel immediately, but never answers VirtIO messages.
///
/// The stub is re-entered at its top after *every* yield (the kernel
/// resumes it at its entry point), while `vm_yield_entry`'s prologue grows
/// the guest stack by one frame per call — so the stub first resets SP to
/// the top of the guest RAM (0x4019_7000 = ram_base + ram_size).  It then
/// reloads the boot args: `vm::Manager` re-establishes x5 = yield function
/// and x6 = guest-context pointer on every resume, because the kernel's own
/// execution clobbers caller-saved registers between yields.
///
///   movz x7, #0x1970, lsl #16   ; x7 = 0x40197000 (guest stack top)
///   movk x7, #0x40,   lsl #32
///   mov  sp, x7
///   mov  x0, x6                  ; guest-context pointer (set by Manager)
///   br   x5                      ; jump to the kernel's vm_yield_entry
///
/// The kernel's demo loop then completes with "no Echo" warnings instead of
/// hanging.
#[cfg(not(feature = "embed-zephyr-stub"))]
static ZEPHYR_STUB_BIN: &[u8] = &[
    0x07, 0x2e, 0xa3, 0xd2, // movz x7, #0x1970, lsl #16
    0x07, 0x08, 0xc0, 0xf2, // movk x7, #0x40,   lsl #32
    0xff, 0x00, 0x00, 0x91, // mov  sp, x7
    0xe0, 0x03, 0x06, 0xaa, // mov  x0, x6
    0xa0, 0x00, 0x1f, 0xd6, // br   x5
];

// ── Boot entry ────────────────────────────────────────────────────────────────
//
// `_start` is pure assembly: at reset the CPU may be in EL3 or EL2 (QEMU's
// `virt` machine with `virtualization=on` hands `-kernel` the CPU at EL2),
// while the whole kernel targets EL1.  We therefore drop EL3 -> EL2 -> EL1
// and only then set SP (the reset value is 0, so any stack use before this
// point faults) and jump into Rust.

use core::arch::global_asm;

global_asm!(
    r#"
    .section .text._start, "ax"
    .global _start
_start:
    // Determine the current exception level.
    mrs  x0, CurrentEL
    and  x0, x0, #0xc
    cmp  x0, #0xc
    b.eq 3f
    cmp  x0, #0x8
    b.eq 2f
    b    1f

3:  // EL3 -> EL2: SCR_EL3 with NS=1, HCE=1, RW=1
    mov  x1, #0x501
    msr  SCR_EL3, x1
    isb
    adr  x1, 2f
    msr  ELR_EL3, x1
    mov  x1, #0x3c9      // SPSR: EL2h, DAIF masked
    msr  SPSR_EL3, x1
    eret

2:  // EL2 -> EL1: HCR_EL2.RW=1 forces AArch64 at EL1.
    mov  x1, #1
    lsl  x1, x1, #31
    msr  HCR_EL2, x1
    msr  SCTLR_EL2, xzr
    isb
    adr  x1, 1f
    msr  ELR_EL2, x1
    mov  x1, #0x3c5      // SPSR: EL1h, DAIF masked
    msr  SPSR_EL2, x1
    eret

1:  // EL1h: allow SIMD/FP at EL1 (CPACR_EL1.FPEN=3) — the server tasks
    // use NEON in libtanix-sys (e.g. buffer zeroing in `log`).
    mov  x1, #3
    lsl  x1, x1, #20
    msr  CPACR_EL1, x1
    isb

    // Initialise the stack, then enter Rust.
    adrp x0, __stack_top
    add  x0, x0, :lo12:__stack_top
    mov  sp, x0
    b    kmain_entry
    "#
);

/// Rust-side boot continuation, reached from the `_start` stub with a valid
/// stack and SP pointing at `__stack_top`.  Zeroes BSS and enters `kmain`.
#[no_mangle]
pub extern "C" fn kmain_entry() -> ! {
    unsafe {
        extern "C" {
            static __bss_start: u8;
            static __bss_end:   u8;
            static __stack_top: u8;
        }
        let bss_start = core::ptr::addr_of!(__bss_start) as *mut u8;
        let bss_len   = (core::ptr::addr_of!(__bss_end) as usize)
            .saturating_sub(bss_start as usize);
        core::ptr::write_bytes(bss_start, 0, bss_len);
        core::arch::asm!(
            "mov sp, {top}",
            top = in(reg) core::ptr::addr_of!(__stack_top) as usize,
            options(nomem, nostack)
        );
    }
    kmain();
}

// ── Kernel main ───────────────────────────────────────────────────────────────

fn kmain() -> ! {
    // ── Phase 1: hardware ────────────────────────────────────────────────────
    arch::aarch64::init();

    // ── Phase 2: memory + hypervisor ─────────────────────────────────────────
    let kernel_end = {
        extern "C" { static __kernel_end: u8; }
        core::ptr::addr_of!(__kernel_end) as usize
    };
    log::info!("kernel image ends at {:#x}", kernel_end);

    unsafe { mem::frame::init(kernel_end); }
    unsafe { mem::page_table::enable(); }

    // Reserve the server regions in the frame allocator *before* any
    // dynamic allocation (shmem, guest RAM, framebuffers): the regions are
    // fixed addresses (the servers link there), so nobody else may receive
    // frames that overlap a live server image.
    if server::available() {
        server::reserve_regions();
    }

    let hv = hypervisor::detect_backend();

    // ── Phase 3: VirtIO shared-memory transport ───────────────────────────────
    //
    // Memory layout after the kernel image (all in the 256 MiB DDR window,
    // which the MMU pre-maps):
    //   [shmem]       4 pages (16 KiB)   — VirtQueue + data buffers
    //   [guest RAM]   256 pages (1 MiB)  — Zephyr stub text + data + stack
    //
    // The shmem base is passed to the guest in x4 at launch; the guest's
    // VirtIO driver finds its VirtqueueConfig there.

    // 1. Allocate and share the VirtQueue region.
    let shmem_handle = unsafe {
        vm::shmem::alloc_shmem(4, hv)
            .expect("phase 3: failed to allocate shmem")
    };
    let shmem_phys = unsafe {
        vm::shmem::region_for(shmem_handle)
            .expect("phase 3: shmem not found")
            .phys
    };
    log::info!("phase 3: shmem at {:#x}", shmem_phys);

    // 2. Register the inter-VM doorbell (SGI 1, VM handle = 1).
    //    Retained for the Gunyah path; the cooperative demo communicates
    //    via the yield pair instead of interrupts.
    let doorbell = hypervisor::doorbell::register(1, 1)
        .expect("phase 3: failed to register doorbell");

    // 3. Initialise the VirtIO transport (writes VirtqueueConfig into shmem).
    let mut transport = unsafe {
        virtio::transport::VirtioTransport::new(shmem_phys, doorbell)
    };
    log::info!("phase 3: VirtIO transport ready");

    // 4. Create and launch the Zephyr guest VM.
    //    Boot args: x4 = shmem base, x5 = kernel yield function.  The guest
    //    uses the yield function to hand control back between messages.
    const GUEST_RAM_PAGES: usize = 256; // 1 MiB
    let boot_args: [u64; 2] = [
        shmem_phys as u64,
        vm::yield_fn_addr() as u64,
    ];
    let guest_handle = unsafe {
        vm::create_vm("zephyr-stub", ZEPHYR_STUB_BIN, GUEST_RAM_PAGES, hv, boot_args)
            .expect("phase 3: failed to create guest VM")
    };

    log::info!(
        "phase 3: launching guest VM {:?} (shmem={:#x})",
        guest_handle, shmem_phys
    );

    // 5. Enter the guest.  This switches the CPU into the guest; it returns
    //    when the guest yields control back (right after its boot banner).
    unsafe {
        vm::start_vm(guest_handle, hv)
            .expect("phase 3: vm_start failed");
    }

    // ── Phase 3: drive the VirtIO ping-pong ──────────────────────────────────
    //
    // Each round: post a Print message → resume the guest (it processes the
    // message, writes an Echo into the used ring, yields) → collect the
    // reply.  Fully synchronous and race-free: the guest cannot run while
    // we are preparing the next message, and vice-versa.

    log::info!("phase 3: starting VirtIO message exchange");

    for round in 0u32..3 {
        let messages: [&[u8]; 3] = [
            b"Hello from Tanix kernel (round 0)!",
            b"Hello from Tanix kernel (round 1)!",
            b"Hello from Tanix kernel (round 2)!",
        ];
        let text = messages[round as usize];

        unsafe {
            transport.send_print(text, hv);

            vm::resume_vm(guest_handle, hv)
                .expect("phase 3: vm_resume failed");

            let mut got_reply = false;
            transport.poll_replies(|desc_idx, op, printed| {
                log::info!(
                    "virtio: reply desc={} op={:?} printed={}",
                    desc_idx, op, printed
                );
                got_reply = true;
            });

            if got_reply {
                log::info!("phase 3: round {} — Echo received", round);
            } else {
                log::warn!(
                    "phase 3: round {} — no Echo (guest not answering)",
                    round
                );
            }
        }
    }

    log::info!("phase 3: VirtIO transport demo complete");

    // ── Phase 4: Minix-style server processes ─────────────────────────────────
    //
    // The kernel now boots a set of independent server binaries (init, pm,
    // mem, dev, worker) as cooperative tasks, each in its own memory
    // region with its own stack.  Servers never link against the kernel —
    // they communicate only through the syscall table handed to them at
    // boot (x19) and through synchronous `send` / `receive` messages.
    //
    // Flow: kmain spawns `init` and enters the scheduler; `init` spawns
    // pm/mem/dev, exercises each service over IPC (dev prints, mem allocs,
    // pm execs the worker), then exits.  When every server has blocked or
    // finished, the scheduler returns here.

    log::info!("phase 4: booting Minix-style server set");

    if server::available() {
        match server::spawn_by_name("init") {
            Ok(id) => {
                log::info!("phase 4: init server spawned as {:?}", id);
                unsafe {
                    sched::enter();
                }
                log::info!("phase 4: Minix-style server demo complete");
            }
            Err(e) => {
                log::error!("phase 4: failed to spawn init server (err={})", e);
            }
        }
    } else {
        log::warn!(
            "phase 4: server binaries not embedded \
             (build with --features embed-servers)"
        );
    }

    // ── Phase 5: display stack ────────────────────────────────────────────────
    //
    // The kernel boots the display server (owns the QEMU virtio-gpu
    // framebuffer + virtio-tablet pointer).  The Phase-4 demo needs `init`
    // to exit, so it can no longer act as the root for the display-stack
    // servers; kmain spawns them directly once the Phase-4 demo has run its
    // course.  The Phase-8 window manager and the apps are spawned after
    // this block; `display` is highest-priority (32) so it initialises the
    // GPU before wm or any app runs.

    if server::available() {
        let display = server::spawn_by_name("display");
        log::info!(
            "phase 5: display server spawned (display={:?})",
            display
        );
    }

    // ── Phase 8: window manager + multiple concurrent apps ───────────────────
    //
    // `wm` (window manager / compositor, priority 48) owns the window
    // table: apps create windows (off-screen canvases they draw into),
    // flush them to the scanout, and receive pointer events routed into
    // their window coordinates.  `wm` handles placement, z-order
    // (click-to-raise), dragging via the title bar, and composites the
    // desktop through the display server.
    //
    // Three demo apps run concurrently (priority 96): `ui-demo` (paint),
    // `counter` and `clock`.  `hog` (192) spins in the background; the
    // preemptive tick (armed below) keeps everything moving and wakes
    // `clock`'s SYS_SLEEP deadlines.

    if server::available() {
        let wm = server::spawn_by_name("wm");
        let ui_demo = server::spawn_by_name("ui-demo");
        let counter = server::spawn_by_name("counter");
        let clock = server::spawn_by_name("clock");
        let hog = server::spawn_by_name("hog");
        log::info!(
            "phase 8: window stack spawned (wm={:?}, ui-demo={:?}, counter={:?}, clock={:?}, hog={:?})",
            wm, ui_demo, counter, clock, hog
        );

        // Enable the EL1 physical-timer interrupt (PPI 30) and arm the
        // periodic 1 ms tick (preemption + SYS_SLEEP wake-ups).
        arch::aarch64::gic::enable_irq(30);
        arch::aarch64::timer::init_tick();
        log::info!("phase 8: preemption tick armed (PPI 30)");

        unsafe {
            sched::enter();
        }
        log::info!("phase 8: window stack idle (unreachable — hog never blocks)");
    } else {
        log::warn!(
            "phase 8: server binaries not embedded \
             (build with --features embed-servers)"
        );
    }

    // ── Idle ─────────────────────────────────────────────────────────────────
    arch::aarch64::halt();
}
