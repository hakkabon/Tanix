#![no_std]
#![no_main]

mod arch;
mod hypervisor;
mod ipc;
mod mem;
mod panic;
mod sched;
mod virtio;
mod vm;

// ── Guest VM binary ───────────────────────────────────────────────────────────

/// Phase 3 Zephyr-stub with VirtIO driver, embedded at compile time.
/// Build with `just zephyr-stub` before `just kernel-embed`.
#[cfg(feature = "embed-zephyr-stub")]
static ZEPHYR_STUB_BIN: &[u8] = include_bytes!(
    "../../target/aarch64-unknown-none/debug/tanix-zephyr-stub"
);

/// Fallback: tight WFI loop — boots but does not participate in VirtIO.
#[cfg(not(feature = "embed-zephyr-stub"))]
static ZEPHYR_STUB_BIN: &[u8] = &[
    0x7f, 0x20, 0x03, 0xd5, // wfi
    0xff, 0xff, 0xff, 0x17, // b #-4
];

// ── Boot entry ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn _start() -> ! {
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
    unsafe { mem::page_table::enable(kernel_end); }

    let hv = hypervisor::detect_backend();

    // ── Phase 3: VirtIO shared-memory transport ───────────────────────────────
    //
    // Memory layout after kernel image:
    //   [shmem]       4 pages (16 KiB)   — VirtQueue + data buffers
    //   [guest RAM]   256 pages (1 MiB)  — Zephyr stub text + data + stack
    //
    // The shmem region is allocated first so its address is lower than the
    // guest RAM; we pass the shmem base to the guest via a register on launch.

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
    let doorbell = hypervisor::doorbell::register(1, 1)
        .expect("phase 3: failed to register doorbell");

    // 3. Initialise the VirtIO transport (writes VirtqueueConfig into shmem).
    let mut transport = unsafe {
        virtio::transport::VirtioTransport::new(shmem_phys, doorbell)
    };
    log::info!("phase 3: VirtIO transport ready");

    // 4. Create and launch the Zephyr guest VM.
    //    Pass the shmem physical address in x1 at launch so the guest's
    //    driver can find the VirtqueueConfig without a fixed address.
    const GUEST_RAM_PAGES: usize = 256; // 1 MiB
    let guest_handle = unsafe {
        vm::create_vm("zephyr-stub", ZEPHYR_STUB_BIN, GUEST_RAM_PAGES, hv)
            .expect("phase 3: failed to create guest VM")
    };

    // Pass shmem_phys to the guest.  The bare-metal backend's launch_guest
    // uses `eret`; we load shmem_phys into x1 before calling it.
    unsafe {
        core::arch::asm!(
            "mov x1, {shmem}",
            shmem = in(reg) shmem_phys as u64,
            options(nomem, nostack)
        );
    }

    log::info!("phase 3: launching guest VM {:?} (shmem={:#x})", guest_handle, shmem_phys);

    // vm_start eretes into the guest.  On first HVC the guest context is
    // restored and sync_handler runs in our exception vector.
    unsafe {
        vm::start_vm(guest_handle, hv)
            .expect("phase 3: vm_start failed");
    }

    // ── Phase 3: drive the VirtIO ping-pong ──────────────────────────────────
    //
    // After vm_start returns (the guest halted), the transport is still live.
    // In practice with `eret` the guest runs cooperatively — each time it
    // issues an HVC for a doorbell the exception handler runs synchronously
    // and returns, allowing the guest to continue.
    //
    // Here we run 3 rounds of Print → wait for Echo reply.

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

            let got = transport.wait_reply(500, hv);
            if got {
                log::info!("phase 3: round {} — Echo received", round);
            } else {
                log::warn!("phase 3: round {} — timeout waiting for Echo", round);
            }
        }
    }

    log::info!("phase 3: VirtIO transport demo complete");

    // ── Idle ─────────────────────────────────────────────────────────────────
    arch::aarch64::halt();
}
