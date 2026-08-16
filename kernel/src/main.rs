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
mod smp;
mod sync;
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
//
// Phase 16: the `sbsa-ref` machine resets every CPU at EL3 with QEMU's
// PSCI disabled — the kernel's EL3 monitor is the firmware.  Its `_start`
// saves the DTB pointer (x0) into x24, runs the monitor (`monitor_el3_init`)
// from the EL3 stack, and the primary continues at EL1 here with the DTB
// still in x24, passed to `kmain_entry`.

use core::arch::global_asm;

#[cfg(not(feature = "sbsa-ref"))]
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
    // Phase 18: when UEFI firmware starts the kernel as an EFI app, x1
    // holds the EFI system table pointer (0 under QEMU -kernel).  Stash
    // it before clobbering x1 — efi::handoff reads it after BSS zeroing.
    adrp x2, EFI_SYSTAB
    add  x2, x2, :lo12:EFI_SYSTAB
    str  x1, [x2]
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
    // Phase 18: a firmware that starts the EFI app at EL1 hands control
    // with the EL1 MMU *on* and x1 = system table.  Detect that (SCTLR_EL1
    // bit 0 = M), stash x1, and drop the firmware stage-1 translation
    // before the kernel's own MMU config takes over.  Under `-kernel`
    // SCTLR_EL1 is already 0 and x1 is the EL1 return address — skip.
    mrs  x9, SCTLR_EL1
    tbz  x9, #0, 4f
    adrp x2, EFI_SYSTAB
    add  x2, x2, :lo12:EFI_SYSTAB
    str  x1, [x2]
    msr  SCTLR_EL1, xzr
    isb
4:
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

#[cfg(feature = "sbsa-ref")]
global_asm!(
    r#"
    .section .text._start, "ax"
    .global _start
_start:
    // Save the DTB pointer (x0 from QEMU) before touching any register.
    // Phase 18: when UEFI starts this image x0 is the image handle instead
    // — the EL2/EL1 paths below treat x0 as junk and pass 0 as the DTB.
    mov  x24, x0
    mrs  x9, CurrentEL
    and  x9, x9, #0xc
    cmp  x9, #0xc
    b.eq 3f
    cmp  x9, #0x8
    b.eq 2f
    b    1f

3:  // EL3 reset (-kernel): run the EL3 monitor.  At EL3 with SPsel=0:
    // switch to SP_EL3 and set up the monitor stack.
    msr  SPSel, #1
    adrp x0, __el3_stack_top
    add  x0, x0, :lo12:__el3_stack_top
    mov  sp, x0
    // Call the EL3 monitor: (dtb, is_secondary, el1_entry).  It erets to
    // `1:` below (primary) or to the EL3 park loop (secondary).
    // QEMU resets every CPU at this vector; CPU0 is the primary, the
    // others (Aff0 != 0) park in `monitor_park_loop` until the kernel
    // wakes them with a PSCI CPU_ON command slot + SGI.
    adr  x2, 1f
    mov  x0, x24
    mrs  x1, MPIDR_EL1
    and  x1, x1, #0xff
    cmp  x1, #0
    cset x1, ne
    bl   monitor_el3_init

2:  // EL2 entry (UEFI app): stash x1 = EFI system table, drop to EL1.
    // Idle cores that QEMU's -kernel reset in ... (not reached at reset:
    // secondaries enter at `secondary_entry`).
    adrp x2, EFI_SYSTAB
    add  x2, x2, :lo12:EFI_SYSTAB
    str  x1, [x2]
    mov  x1, #1
    lsl  x1, x1, #31
    msr  HCR_EL2, x1
    msr  SCTLR_EL2, xzr
    isb
    adr  x1, 1f
    msr  ELR_EL2, x1
    mov  x1, #0x3c5      // SPSR: EL1h, DAIF masked
    msr  SPSR_EL2, x1
    mov  x1, xzr         // `1:` below re-stashes x1 — keep the saved table
    eret

1:  // NS EL1h continuation (primary only): SIMD/FP + kernel stack.
    // Phase 18: x1 is the EFI system table when a UEFI firmware started
    // us (either direct at EL1, or via the EL2 leg above whose final eret
    // leaves x1 = 0), and 0 on the EL3 `-kernel` path (the EL3 monitor
    // zeroes x1 before its own final eret).  Stash it before the firmware
    // stage-1 MMU is dropped — this must not be conditional on the MMU
    // state (SbsaQemu's EDK2 enters the app at EL1 with it off).
    adrp x2, EFI_SYSTAB
    add  x2, x2, :lo12:EFI_SYSTAB
    str  x1, [x2]
    mrs  x9, SCTLR_EL1
    tbz  x9, #0, 4f
    msr  SCTLR_EL1, xzr
    isb
4:  // (TEMP DEBUG: announce the EL we actually landed at on the NS PL011.)
    mrs  x9, CurrentEL
    and  x9, x9, #0xc
    lsr  x9, x9, #2
    add  x9, x9, #0x30
    movz x10, #0x1000, lsl #16   // UART base 0x60000000 (sbsa NS PL011)
    movk x10, #0x6000, lsl #16
    add  x10, x10, #0x18         // FR
2:  ldr  w11, [x10]
    tbnz w11, #5, 2b             // wait for TX FIFO not full
    sub  x10, x10, #0x18         // DR
    strb w9, [x10]
    mov  x1, #3
    lsl  x1, x1, #20
    msr  CPACR_EL1, x1
    isb
    adrp x0, __stack_top
    add  x0, x0, :lo12:__stack_top
    mov  sp, x0
    mov  x0, x24         // DTB still valid — hand it to kmain_entry
    b    kmain_entry
    "#
);

// ── Secondary-CPU entry (Phase 11 SMP) ─────────────────────────────────────────
//
// QEMU's PSCI CPU_ON starts secondaries at EL2 with PC = this symbol (our
// EL3 monitor starts them at EL1 directly).  They drop EL2 -> EL1 (the
// same sequence as `_start`'s `2:` path), select their own boot stack from
// `SECONDARY_STACKS` (indexed by MPIDR Aff0), and continue in
// `kmain_secondary_entry`.  BSS is already zeroed by the primary —
// secondaries must NOT re-zero it.

global_asm!(
    r#"
.section .text._start_secondary, "ax"
.global secondary_entry
.type secondary_entry, %function
secondary_entry:
    // The EL1 MMU state left by PSCI CPU_ON is not guaranteed to match the
    // kernel's boot config: the secondary can start with SCTLR_EL1.M=1 and a
    // stale TTBR0_EL1 (QEMU hands over the caller's EL1 sysregs), so the
    // kernel image / UART stay mapped but the per-CPU boot stack is not —
    // which faults the very first `stp`.  Disable the EL1 MMU before touching
    // any memory (register-only, no memory access): the whole EL1 boot then
    // runs on physical identity addresses and kmain_secondary_entry re-enables
    // the MMU with the kernel table via mmu::init() + enable_secondary().
    msr  SCTLR_EL1, xzr
    isb
    mrs  x0, CurrentEL
    and  x0, x0, #0xc
    cmp  x0, #0xc
    b.eq 3f
    cmp  x0, #0x8
    b.eq 2f
    b    1f

3: // EL3 -> EL2 (defensive — QEMU already starts secondaries at EL2).
    mov  x1, #0x501
    msr  SCR_EL3, x1
    isb
    adr  x1, 2f
    msr  ELR_EL3, x1
    mov  x1, #0x3c9
    msr  SPSR_EL3, x1
    eret

2: // EL2 -> EL1: HCR_EL2.RW=1 forces AArch64 at EL1.
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

1: // EL1h: SIMD/FP at EL1.
    mov  x1, #3
    lsl  x1, x1, #20
    msr  CPACR_EL1, x1
    isb

    // Per-CPU stack: SECONDARY_STACKS + (cpu-1) * 0x10000, top.
    mrs  x2, MPIDR_EL1
    and  x2, x2, #0xff     // cpu index (Aff0)
    sub  x2, x2, #1
    adrp x0, SECONDARY_STACKS
    add  x0, x0, :lo12:SECONDARY_STACKS
    movz x1, #1, lsl #16   // stack size = 0x10000
    madd x0, x2, x1, x0
    add  sp, x0, x1
    // Install the vector table (VBAR_EL1) here so an early fault panics
    // with ESR/ELR instead of looping on the zeroed vectors at 0x0.
    adrp x0, __vectors
    add  x0, x0, :lo12:__vectors
    msr  VBAR_EL1, x0
    isb
    b    kmain_secondary_entry
    "#
);

/// Rust-side boot continuation, reached from the `_start` stub with a valid
/// stack and SP pointing at `__stack_top`.  Zeroes BSS and enters `kmain`.
///
/// Phase 16: `dtb` is the flattened device-tree pointer passed by QEMU in
/// x0 (both machines); on `virt` it is 0 when QEMU was started without a
/// DT (the machine table supplies the defaults).
#[no_mangle]
pub extern "C" fn kmain_entry(dtb: usize) -> ! {
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
    kmain(dtb);
}

/// Rust-side continuation for secondary CPUs, reached from `secondary_entry`
/// at EL1h with SP on the CPU's own `SECONDARY_STACKS` region.  BSS is
/// already zeroed by the primary, so no re-zeroing here.
///
/// Each secondary re-arms the per-CPU hardware (MMU, vectors, GIC
/// redistributor, timer tick) and then idles in its own scheduler slot,
/// competing for tasks on the global runqueue.
#[no_mangle]
pub extern "C" fn kmain_secondary_entry() -> ! {
    // Install vectors first so an early fault panics with ESR/ELR instead
    // of looping on the zeroed vector table at VBAR=0.
    arch::aarch64::exception::init();

    let cpu = smp::cpu_index();
    log::info!(
        "smp: CPU {} up at EL1 (mpidr={:#x})",
        cpu,
        arch::aarch64::boot::mpidr()
    );

    // Per-CPU MMU state: TCR/MAIR (reset to 0 on a fresh CPU), TTBR0
    // (kernel identity map) and SCTLR_EL1.M — the identity map means all
    // kernel addresses equal physical addresses, exactly like CPU 0.
    arch::aarch64::mmu::init();
    unsafe { mem::page_table::enable_secondary(); }

    arch::aarch64::exception::init(); // VBAR_EL1 (reset value is 0)
    arch::aarch64::gic::init();       // this CPU's redistributor + ICC
    arch::aarch64::timer::init();     // disarm until the tick is armed

    // Preemption tick on this CPU: PPI 30 is per-CPU by definition.
    arch::aarch64::gic::enable_irq(30);
    arch::aarch64::timer::init_tick();
    log::info!("smp: CPU {} tick armed", cpu);

    unsafe { sched::secondary_enter(cpu) }
}

// ── Kernel main ───────────────────────────────────────────────────────────────

fn kmain(dtb: usize) -> ! {
    // ── Phase 18: firmware handoff ──────────────────────────────────────────
    //
    // When UEFI loaded this image, `_start` stashed the EFI system table
    // and this is what we want back from the firmware: the ACPI RSDP (the
    // real "how the platform describes itself" on FVP/real boards) and a
    // device tree if the firmware carries one.  The ACPI parse runs before
    // the UART is up (no logging here); its outcome is logged after init.
    let efi = arch::aarch64::efi::handoff();
    let dtb = if efi.booted_from_efi { efi.dtb } else { dtb };
    let acpi = if efi.rsdp != 0 {
        arch::aarch64::acpi::probe(efi.rsdp)
    } else {
        None
    };
    if let Some(info) = &acpi {
        arch::aarch64::machine::set_from_acpi(info);
    }

    // ── Phase 1: hardware ────────────────────────────────────────────────────
    arch::aarch64::init();

    if efi.booted_from_efi {
        log::info!(
            "phase 18: UEFI boot — system table {:#x}, rsdp {:#x}, dtb {:#x}",
            efi.systab, efi.rsdp, dtb
        );
        match &acpi {
            Some(i) => log::info!(
                "phase 18: ACPI machine — GICD={:#x} GICR={:#x} ITS={:#x} UART={:#x} ECAM={:#x}",
                i.gic_dist_base, i.gic_redist_base, i.its_base, i.uart_base, i.ecam_base
            ),
            None => log::warn!(
                "phase 18: no usable ACPI tables — falling back to machine defaults"
            ),
        }
    }

    // ── Phase 2: memory + hypervisor ─────────────────────────────────────────
    let kernel_end = {
        extern "C" { static __kernel_end: u8; }
        core::ptr::addr_of!(__kernel_end) as usize
    };
    log::info!("kernel image ends at {:#x}", kernel_end);

    // Phase 16: RAM bounds come from the DT where present (`virt` publishes
    // /memory; `sbsa-ref`'s minimal DT has none, so the machine table wins)
    // and feed the frame allocator + page tables — the whole point of the
    // DT parse: a physical board hands the kernel its memory layout at boot.
    let machine = arch::aarch64::machine();
    let (ram_base, ram_size) = arch::aarch64::fdt::dram_region(dtb)
        .map(|r| (r.base, r.size))
        .unwrap_or((machine.dram_base, machine.dram_size));
    log::info!(
        "machine {}: DDR {:#x}..{:#x} ({} MiB, dtb={:#x})",
        machine.id,
        ram_base,
        ram_base + ram_size,
        ram_size / (1024 * 1024),
        dtb
    );

    unsafe { mem::frame::init(kernel_end, ram_base, ram_size); }
    unsafe { mem::page_table::enable(ram_base, ram_size); }

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

    // ── Phase 14: hypervisor assist — doorbell wakeups ───────────────────────
    //
    // Phase 13 exchanged messages but fused send and resume: the kernel
    // woke the guest by resuming it.  Phase 14 decouples the two with a
    // real doorbell (a zero-payload IRQ):
    //
    //   • the guest *blocks* on an empty receive: it writes guest_state =
    //     WAITING into the shared VMM info block and yields;
    //   • the kernel keeps producing — three sends, ringing the doorbell
    //     after each (GIC SGI 1 on bare metal, GH_BELL_SEND on Gunyah) —
    //     without resuming the guest.  Rings coalesce in the GIC into one
    //     delivery, which the kernel's IRQ handler records;
    //   • the kernel then resumes the guest once: it wakes, drains all
    //     three messages and replies.
    //
    // The guest is also where Phase 13 leaked: it halted inside the final
    // vcpu_run (an infinite spin), so the kernel never reached the Phase-4
    // demo.  Phase 14 finishes with a "done" message the guest
    // acknowledges and then *parks* — handing control back — so the boot
    // sequence continues.
    const VMM_INFO_MAGIC: u32 = 0x564D_4D49; // "IVMM"
    const VMM_INFO_OFF: usize = 0x2000;
    const GUEST_RUNNING: u32 = 0;
    const GUEST_WAITING: u32 = 1;

    let mq = hv
        .msgq_create(guest_handle, 8)
        .expect("phase 14: msgq_create failed");
    let bell = hv
        .doorbell_create(guest_handle, 2) // SGI 2 = phase-14 doorbell
        .expect("phase 14: doorbell_create failed");
    unsafe {
        // VMM info block: u32 magic, u32 msgq, u64 vmm_service,
        // u32 doorbell, u32 guest_state, u32 doorbell_flags.
        let info = (shmem_phys as usize + VMM_INFO_OFF) as *mut u32;
        core::ptr::write_volatile(info, VMM_INFO_MAGIC);
        core::ptr::write_volatile(info.add(1), mq.0);
        core::ptr::write_volatile(
            info.add(2) as *mut u64,
            hypervisor::doorbell::vmm_service as *const () as u64,
        );
        core::ptr::write_volatile(info.add(4), bell.0);
        core::ptr::write_volatile(info.add(5), GUEST_RUNNING);
        core::ptr::write_volatile(info.add(6), 0u32);
    }
    log::info!(
        "phase 14: info block published (msgq={:?} doorbell={:?} service={:#x})",
        mq, bell,
        hypervisor::doorbell::vmm_service as *const () as usize
    );

    // The doorbell is delivered as a real GIC SGI IRQ, so this demo needs
    // IRQs unmasked — everything up to here runs masked (the scheduler
    // tick that unmasks them at phase 9 isn't armed yet).
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack)); // clear I
    }

    // 1. Run the guest once so it reads the info block and blocks on the
    //    empty queue (guest_state → WAITING, then yield).
    unsafe {
        vm::resume_vm(guest_handle, hv)
            .expect("phase 14: first resume failed");
    }
    if guest_info_state(shmem_phys) == GUEST_WAITING {
        log::info!("phase 14: guest asleep on the queue — ringing doorbells");
    } else {
        log::warn!("phase 14: guest unexpectedly not blocked");
    }

    // 2. Produce three messages without resuming the guest, ringing the
    //    doorbell after each send.  The doorbell's whole purpose: the
    //    producer signals without knowing (or caring) whether the
    //    consumer is blocked.  The info-block flags field is the
    //    guest-visible doorbell state (the guest clears it when it wakes).
    for round in 0u32..3 {
        let text: &[u8] = match round {
            0 => b"ping-0",
            1 => b"ping-1",
            _ => b"ping-2",
        };
        hv.msgq_send(mq, text)
            .expect("phase 14: msgq_send failed");
        hv.doorbell_send(bell)
            .expect("phase 14: doorbell_send failed");
        unsafe {
            core::ptr::write_volatile(
                (shmem_phys as usize + VMM_INFO_OFF + 24) as *mut u32,
                1u32, // DOORBELL_FLAG_MSG
            );
        }
    }

    // 3. The rings coalesce in the GIC into one pending SGI (the guest's
    //    vCPU is asleep; the cooperative switch runs it masked, so rings
    //    stay pending — on Gunyah the vCPU would sit in WFI).  Clearing
    //    the I bit is the wakeup: the single pending SGI delivers once and
    //    the handler records it.  Wait for that delivery — the kernel
    //    observes the doorbell event before deciding to run the vCPU.
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack)); // clear I
    }
    while hypervisor::doorbell::stats(bell)
        .map(|(_, d)| d == 0)
        .unwrap_or(true)
    {
        core::hint::spin_loop();
    }
    let (rings, deliveries) = hypervisor::doorbell::stats(bell)
        .expect("phase 14: doorbell stats");
    log::info!(
        "phase 14: doorbell delivered (rings={} deliveries={}, coalesced={})",
        rings, deliveries, rings - deliveries
    );

    // 4. Resume the woken guest once: it drains all three pings and
    //    replies, then blocks on the (now empty) queue again.
    unsafe {
        vm::resume_vm(guest_handle, hv)
            .expect("phase 14: drain resume failed");
    }
    log::info!("phase 14: guest ran and drained the queue");

    // 5. Collect the three pongs.
    for round in 0u32..3 {
        let mut reply = [0u8; hypervisor::MSGQ_MAX_MSG_SIZE];
        match hv.msgq_recv(mq, &mut reply) {
            Ok((n, _)) => {
                let got = core::str::from_utf8(&reply[..n]).unwrap_or("?");
                log::info!("phase 14: round {} — '{}' received", round, got);
            }
            Err(e) => {
                log::warn!("phase 14: round {} — no reply ({:?})", round, e);
            }
        }
    }

    // 6. Finale: one more message wakes the guest, which acknowledges and
    //    parks (this time it hands control back instead of halting inside
    //    the vCPU run, so the Phase-4 demo below still runs).
    hv.msgq_send(mq, b"done")
        .expect("phase 14: finale send failed");
    hv.doorbell_send(bell)
        .expect("phase 14: finale ring failed");
    unsafe {
        core::ptr::write_volatile(
            (shmem_phys as usize + VMM_INFO_OFF + 24) as *mut u32,
            1u32, // DOORBELL_FLAG_MSG
        );
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack)); // clear I
    }
    unsafe {
        vm::resume_vm(guest_handle, hv)
            .expect("phase 14: finale resume failed");
    }
    {
        let mut ack = [0u8; hypervisor::MSGQ_MAX_MSG_SIZE];
        match hv.msgq_recv(mq, &mut ack) {
            Ok((n, _)) => {
                let got = core::str::from_utf8(&ack[..n]).unwrap_or("?");
                log::info!("phase 14: finale ack — '{}'", got);
            }
            Err(e) => log::warn!("phase 14: no finale ack ({:?})", e),
        }
    }
    log::info!(
        "phase 14: guest parked (state={}) — demo complete",
        guest_info_state(shmem_phys)
    );
    let (tot_rings, tot_deliveries) = hypervisor::doorbell::stats(bell)
        .expect("phase 14: doorbell stats");
    log::info!(
        "phase 14: doorbell totals — rings={} deliveries={} (coalesced={})",
        tot_rings, tot_deliveries, tot_rings - tot_deliveries
    );
    log::info!("phase 14: doorbell message-queue demo complete");

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

    // ── Phase 16: EL3 monitor / TrustZone demo ────────────────────────────────
    //
    // On `sbsa-ref` this exercises the real hardware story: the kernel's
    // PSCI and every secure service below cross the EL3 monitor through
    // `smc #0`, the secure payload runs at S-EL1 in TrustZone secure RAM
    // (its own console is QEMU's second `-serial`), and the tick counts
    // are produced by the secure EL1 physical timer (PPI 29, Group 0).
    // On `virt` the same code path runs (the payload executes in place —
    // QEMU's `virt` has no EL3 monitor of our own, so the calls trap to
    // QEMU's emulated PSCI for the PSCI ids and to our monitor-less
    // fallback for the rest; the demo therefore only runs on sbsa-ref).
    // Phase 18: under UEFI the EL3 monitor / TrustZone setup never ran
    // (the EL3-reset boot path installs it), so the SMC demos have no
    // target — skip them and continue with the scheduler phases.
    if arch::aarch64::machine().id == arch::aarch64::machine::MACHINE_SBSA_REF
        && !efi.booted_from_efi
    {
        let kernel_start = {
            extern "C" { static __kernel_start: u8; }
            core::ptr::addr_of!(__kernel_start) as usize
        };
        let local = fnv1a(kernel_start, kernel_end - kernel_start);
        let measured = arch::aarch64::monitor::secure_measure_smc(kernel_start, kernel_end - kernel_start);
        log::info!(
            "phase 16: TCB measurement of kernel image \
             ({:#x}..{:#x}): monitor={:#x} local={:#x} {}",
            kernel_start, kernel_end, measured, local,
            if measured == local { "match" } else { "MISMATCH" }
        );
        log::info!("phase 16: PSCI version via monitor: {:#x}", arch::aarch64::monitor::psci_version_smc());
        log::info!("phase 16: secure monotonic counter: {}", arch::aarch64::monitor::secure_counter_incr());
        arch::aarch64::monitor::secure_banner();
        // Two secure-world sessions: the payload counts secure timer ticks
        // (~50 ms each) for 2500 loop iterations, then hands back.
        let ticks = arch::aarch64::monitor::world_switch(2_500, 1);
        log::info!("phase 16: secure world ran and counted {} ticks", ticks);
        log::info!(
            "phase 16: secure tick counter now {}",
            arch::aarch64::monitor::secure_tick_get()
        );
    }

    // ── Phase 17: secure services demo (storage / keybox / attestation) ─────
    //
    // The `sec` server exercises the EL3 monitor's services end-to-end:
    // secure storage PUT/GET, keybox seal/unseal, and an attestation
    // quote of its *own* image (digest + EL3-secret keyed MAC, nonce
    // bound).  sbsa-ref only — `virt` has no EL3 monitor, and the server
    // parks with a log line there.
    if arch::aarch64::machine().id == arch::aarch64::machine::MACHINE_SBSA_REF
        && !efi.booted_from_efi
    {
        match crate::server::spawn_by_name_locked("sec") {
            Ok(id) => log::info!("phase 17: spawned secure-services server as task {:?}", id),
            Err(e) => log::warn!("phase 17: could not spawn secure-services server ({})", e),
        }
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
    //
    // Phase 16: the whole display family is `virt`-only — `sbsa-ref` has
    // no virtio-mmio transports, and the `display`/`wm`/`shell` images
    // probe the QEMU-virt MMIO window they are built for.

    if server::available() && !arch::aarch64::machine::is_sbsa_ref() {
        let display = server::spawn_by_name("display");
        log::info!(
            "phase 5: display server spawned (display={:?})",
            display
        );
    }

    // ── Phase 8/9/10: window manager, ramfs, shell, net + app registry ──────
    //
    // `wm` (window manager / compositor, priority 48) owns the window
    // table: apps create windows (off-screen canvases they draw into),
    // flush them to the scanout, and receive pointer events routed into
    // their window coordinates.  `wm` handles placement, z-order
    // (click-to-raise), dragging via the title bar, and composites the
    // desktop through the display server.
    //
    // Phase 9: `ramfs` (64) serves the embedded file tree (the app
    // registry under /bin, text files under /etc) and `shell` (96) opens
    // the terminal window — the user types `exec <app>` to start an app
    // from the kernel's embedded-image registry (counter, clock, ui-demo,
    // hog) with exec-replacement semantics: a running instance is retired
    // first, because every app image links at one fixed address.  `hog`
    // (192) spins in the background; the preemptive tick (armed below)
    // keeps everything moving and wakes `clock`'s SYS_SLEEP deadlines.
    //
    // Phase 10: `net` (96) drives the virtio-net-pci NIC (modern virtio,
    // INTx SPI 36) through the SYS_MAP_DEVICE / SYS_IRQ_PENDING syscalls
    // and runs a tiny ARP/ICMP demo against slirp's 10.0.2.2 gateway.
    //
    // Phase 12: `ping` / `pong` (both 96) run a tight cross-CPU IPC
    // ping/pong stress loop (blocking send/receive rendezvous + payload
    // checksums) — on an SMP boot they migrate across cores, exercising
    // the scheduler lock, the wakeup pokes and the atomic IRQ-pending
    // bits every round.

    if server::available() {
        // Phase 16: on `sbsa-ref` skip the display-family servers (no
        // virtio-mmio, no GPU) — net/hog/ping/pong are machine-agnostic.
        let is_virt = !arch::aarch64::machine::is_sbsa_ref();
        if is_virt {
            let wm = server::spawn_by_name("wm");
            let ramfs = server::spawn_by_name("ramfs");
            let shell = server::spawn_by_name("shell");
            log::info!(
                "phase 9: desktop servers spawned (wm={:?}, ramfs={:?}, shell={:?})",
                wm, ramfs, shell
            );
        }
        let net = server::spawn_by_name("net");
        let hog = server::spawn_by_name("hog");
        let ping = server::spawn_by_name("ping");
        let pong = server::spawn_by_name("pong");
        log::info!(
            "phase 9/10: core servers spawned (net={:?}, hog={:?}, ping={:?}, pong={:?})",
            net, hog, ping, pong
        );

        // Enable the EL1 physical-timer interrupt (PPI 30) and arm the
        // periodic 1 ms tick (preemption + SYS_SLEEP wake-ups).
        arch::aarch64::gic::enable_irq(30);
        arch::aarch64::timer::init_tick();
        log::info!("phase 9: preemption tick armed (PPI 30)");

        // ── Phase 11: SMP bring-up ──────────────────────────────────────────
        // Release the secondary cores via PSCI CPU_ON (they drop EL2→EL1,
        // arm their own ticks and idle on the global runqueue).  Boot with
        // `-smp 4` to activate them; a smaller `-smp` degrades gracefully
        // (CPU_ON returns INVALID_PARAMS).
        smp::bring_up();

        unsafe {
            sched::enter();
        }
        log::info!("phase 9: desktop stack idle (unreachable — hog never blocks)");
    } else {
        log::warn!(
            "phase 9: server binaries not embedded \
             (build with --features embed-servers)"
        );
    }

    // ── Idle ─────────────────────────────────────────────────────────────────
    arch::aarch64::halt();
}

/// Phase 14: read the guest's block state from the shared VMM info block
/// (offset 0x14: u32 magic, u32 msgq, u64 service, u32 doorbell, u32 state,
/// u32 flags).  The guest writes WAITING before yielding on an empty
/// receive; the kernel polls this between resumes.
fn guest_info_state(shmem_phys: usize) -> u32 {
    const VMM_INFO_OFF: usize = 0x2000;
    const STATE_OFF: usize = 20;
    unsafe {
        core::ptr::read_volatile(
            (shmem_phys as usize + VMM_INFO_OFF + STATE_OFF) as *const u32,
        )
    }
}

/// Phase 16: FNV-1a 64 over a memory region — the *local* reference hash
/// the kernel computes to verify the EL3 monitor's TCB measurement.
fn fnv1a(base: usize, len: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01B3;
    let mut hash = FNV_OFFSET;
    for i in 0..len {
        let b = unsafe { core::ptr::read_volatile((base + i) as *const u8) };
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
