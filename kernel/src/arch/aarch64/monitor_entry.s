// Phase 16 — EL3 monitor (TrustZone): vectors, SMC dispatch, world switch.
//
// Runs at EL3 with the MMU off (physical = link addresses).  The non-secure
// kernel calls it with `smc #0`; the secure payload calls it to hand back.
// On entry the monitor saves the caller's FULL context (x0..x30, sp_el1,
// ELR_EL3, SPSR_EL3) into a per-CPU per-world slot (the kernel's inline
// asm declares no clobbers), dispatches, and restores everything (ELR +4
// so the caller resumes after its `smc`).
//
// Context slots (Rust statics):  __ns_ctx[cpu],  __sec_ctx[cpu]
//   layout: x0..x30 @ 0..248, sp_el1 @ 248, elr @ 256, spsr @ 264  (272 B)
// PSCI command slots:          __psci_cmd_slots[cpu] = ( magic, entry )
// Secure payload runtime base: __sec_runtime_base (Rust-owned)

    .section .text.monitor, "ax"

    // ── EL3 vector table ─────────────────────────────────────────────────
    // Only slot 8 (lower EL A64 synchronous — SMC64) is expected; the rest
    // record the fault and hang.
    .align 11
    .global __el3_vectors
__el3_vectors:
    .org 0x000
    b   el3_error
    .org 0x080
    b   el3_error
    .org 0x100
    b   el3_error
    .org 0x180
    b   el3_error
    .org 0x200
    b   el3_error
    .org 0x280
    b   el3_error
    .org 0x300
    b   el3_error
    .org 0x380
    b   el3_error
    .org 0x400
    b   smc_el3_entry
    .org 0x480
    b   el3_error
    .org 0x500
    b   el3_error
    .org 0x580
    b   el3_error
    .org 0x600
    b   el3_error
    .org 0x680
    b   el3_error
    .org 0x700
    b   el3_error
    .org 0x780
    b   el3_error

    // ── Unexpected EL3 exception: record ESR/ELR, hang ───────────────────
el3_error:
    mrs  x0, ESR_EL3
    mrs  x1, ELR_EL3
    adrp x2, __el3_fault
    add  x2, x2, :lo12:__el3_fault
    stp  x0, x1, [x2]
1:  wfi
    b    1b

    // ── SMC entry (slot 8) ───────────────────────────────────────────────
    // Register discipline:
    //   x9  = caller world (1 = NS) — rebuilt at dispatch
    //   x14 = cpu index (Aff0)      — survives the save block
    //   x12 = 272 (ctx slot size)   — survives the save block
    //   x11 = caller's ctx pointer  — survives the save block
    // The caller's x9..x14 originals are pushed before clobbering and
    // written into the ctx slot from the stack.
smc_el3_entry:
    sub  sp, sp, #48
    stp  x9,  x10, [sp, #0]
    stp  x11, x12, [sp, #16]
    stp  x13, x14, [sp, #32]
    // locate the ctx slot: caller world + cpu
    mrs  x9, SCR_EL3
    and  x9, x9, #1
    mrs  x14, MPIDR_EL1
    and  x14, x14, #0xff
    adrp x11, __ns_ctx
    add  x11, x11, :lo12:__ns_ctx
    adrp x12, __sec_ctx
    add  x12, x12, :lo12:__sec_ctx
    cmp  x9, #1
    csel x11, x11, x12, eq
    movz x12, #272
    madd x11, x14, x12, x11     // x11 = &ctx[cpu]
    // save the caller's registers (x0..x8 untouched; x9..x14 from stack)
    stp  x0,  x1,  [x11, #0]
    stp  x2,  x3,  [x11, #16]
    stp  x4,  x5,  [x11, #32]
    stp  x6,  x7,  [x11, #48]
    str  x8,  [x11, #64]
    ldp  x9,  x10, [sp, #0]
    stp  x9,  x10, [x11, #72]
    ldp  x9,  x10, [sp, #16]
    stp  x9,  x10, [x11, #88]
    ldp  x9,  x10, [sp, #32]
    stp  x9,  x10, [x11, #104]
    add  sp, sp, #48
    stp  x14, x15, [x11, #112]
    stp  x16, x17, [x11, #128]
    stp  x18, x19, [x11, #144]
    stp  x20, x21, [x11, #160]
    stp  x22, x23, [x11, #176]
    stp  x24, x25, [x11, #192]
    stp  x26, x27, [x11, #208]
    stp  x28, x29, [x11, #224]
    str  x30, [x11, #240]
    mrs  x9,  sp_el1
    str  x9,  [x11, #248]
    mrs  x9,  ELR_EL3
    str  x9,  [x11, #256]
    mrs  x9,  SPSR_EL3
    str  x9,  [x11, #264]
    // DEBUG (temporary): dump the just-saved SMC context
    stp  x11, x12, [sp, #-32]!
    str  x14, [sp, #16]
    mov  x0, x11
    bl   el3_dbg_ctx
    ldr  x14, [sp, #16]
    ldp  x11, x12, [sp], #32
    // dispatch on the caller's world
    mrs  x9, SCR_EL3
    and  x9, x9, #1
    cmp  x9, #1
    b.ne sec_caller
    // ── non-secure caller ──
    ldr  x0, [x11, #0]          // fn id
    ldr  x1, [x11, #8]          // a1
    ldr  x2, [x11, #16]         // a2
    ldr  x3, [x11, #24]         // a3
    movz x4, #0xC400, lsl #32   // PSCI CPU_OFF → park this CPU
    movk x4, #0x0002
    cmp  x0, x4
    b.eq park_cpu
    movz x4, #0x8300, lsl #16   // SEC_WORLD_SWITCH → enter the secure world
    movk x4, #0x0004
    cmp  x0, x4
    b.ne 8f
    mov  x0, x1                // iters
    mov  x1, x2                // round
    bl   monitor_prepare_switch // write the payload's run data, then switch
    b    enter_secure
8:  // Pass the full arg set (x0..x5) to the Rust dispatcher.  x11 (the
    // NS ctx pointer) is caller-saved per AAPCS64, so the Rust code may
    // clobber it — park it on the EL3 stack around the call.
    str  x11, [sp, #-16]!
    ldr  x4, [x11, #32]
    ldr  x5, [x11, #40]
    bl   monitor_smc_dispatch   // x0=fn, x1..x5=args → result in x0
    ldr  x11, [sp], #16
    str  x0, [x11, #0]
    b    restore_ns
    // ── secure caller (the payload's return): hand the result to the NS
    //    kernel and restore the NS context.
sec_caller:
    ldr  x2, [x11, #0]          // payload return value (sec_ctx.regs[0])
    // Restore the NS EL1 MMU state (SCTLR + vectors) saved at enter_secure.
    adrp x5, __el3_saved_sctlr_el1
    add  x5, x5, :lo12:__el3_saved_sctlr_el1
    ldr  x4, [x5]
    msr  SCTLR_EL1, x4
    ldr  x4, [x5, #16]
    msr  VBAR_EL1, x4
    isb
    adrp x3, __ns_ctx
    add  x3, x3, :lo12:__ns_ctx
    madd x11, x14, x12, x3      // &ns_ctx[cpu]
    str  x2, [x11, #0]
    b    restore_ns

    // ── Restore the non-secure caller (resume after its `smc`) ──────────
restore_ns:
    mov  x1, #0x501             // SCR_EL3: NS|HCE|RW (SMD=0 so smc traps)
    msr  SCR_EL3, x1
    ldr  x12, [x11, #248]       // sp_el1
    ldr  x13, [x11, #256]       // elr — QEMU already points at smc+4 here,
                                // so we ERET directly (no extra +4)
    ldr  x14, [x11, #264]       // spsr
    msr  sp_el1, x12
    msr  ELR_EL3, x13
    msr  SPSR_EL3, x14
    ldp  x0,  x1,  [x11, #0]
    ldp  x2,  x3,  [x11, #16]
    ldp  x4,  x5,  [x11, #32]
    ldp  x6,  x7,  [x11, #48]
    ldp  x8,  x9,  [x11, #64]
    ldr  x10, [x11, #80]
    ldp  x12, x13, [x11, #96]
    ldp  x14, x15, [x11, #112]
    ldp  x16, x17, [x11, #128]
    ldp  x18, x19, [x11, #144]
    ldp  x20, x21, [x11, #160]
    ldp  x22, x23, [x11, #176]
    ldp  x24, x25, [x11, #192]
    ldp  x26, x27, [x11, #208]
    ldp  x28, x29, [x11, #224]
    ldr  x30, [x11, #240]
    ldr  x11, [x11, #88]        // x11 restored last — it is the base pointer
    isb
    eret

    // ── World switch: enter the secure world (monitor_prepare_switch has
    //    already written the payload's run data) ─────────────────────────
    // Self-contained: recomputes the sec_ctx pointer and the payload base.
enter_secure:
    // Save the NS EL1 MMU state (SCTLR + vectors) and switch the MMU
    // off: the payload runs at S-EL1 with the MMU disabled (physical
    // security state), while the NS kernel's identity mapping must not
    // be consulted for secure RAM.  VBAR_EL1 is restored on return so a
    // later NS exception never vectors into the payload.
    mrs  x6, SCTLR_EL1
    adrp x5, __el3_saved_sctlr_el1
    add  x5, x5, :lo12:__el3_saved_sctlr_el1
    stp  x6, xzr, [x5]
    mrs  x6, VBAR_EL1
    str  x6, [x5, #16]
    msr  SCTLR_EL1, xzr
    isb
    mrs  x14, MPIDR_EL1
    and  x14, x14, #0xff
    movz x12, #272
    adrp x2, __sec_ctx
    add  x2, x2, :lo12:__sec_ctx
    madd x2, x14, x12, x2        // &sec_ctx[cpu]
    adrp x3, __sec_runtime_base
    add  x3, x3, :lo12:__sec_runtime_base
    ldr  x3, [x3]               // payload base
    // secure world setup
    mov  x1, #0x500             // SCR_EL3 without NS (SMD=0)
    msr  SCR_EL3, x1
    msr  VBAR_EL1, x3           // secure vectors at the payload base
    add  x4, x3, #0x7000        // secure stack top (region-relative)
    msr  SP_EL1, x4
    ldr  x12, [x2, #256]        // sec_ctx.elr — resume point, or...
    cbnz x12, 9f
    add  x12, x3, #0x800        // ...first entry → sec_entry
9:  msr  ELR_EL3, x12
    mov  x13, #0x385            // SPSR_EL3: S-EL1h, F unmasked, I/D/A masked
    msr  SPSR_EL3, x13
    // restore the secure context
    ldp  x0,  x1,  [x2, #0]
    ldp  x4,  x5,  [x2, #32]
    ldp  x6,  x7,  [x2, #48]
    ldp  x8,  x9,  [x2, #64]
    ldp  x10, x11, [x2, #80]
    ldp  x12, x13, [x2, #96]
    ldp  x14, x15, [x2, #112]
    ldp  x16, x17, [x2, #128]
    ldp  x18, x19, [x2, #144]
    ldp  x20, x21, [x2, #160]
    ldp  x22, x23, [x2, #176]
    ldp  x24, x25, [x2, #192]
    ldp  x26, x27, [x2, #208]
    ldp  x28, x29, [x2, #224]
    ldr  x30, [x2, #240]
    ldp  x2,  x3,  [x2, #16]    // x2/x3 last (base register is read first)
    isb
    eret

    // ── PSCI CPU_OFF: park this CPU at EL3 forever ───────────────────────
park_cpu:
1:  wfi
    b    1b

    // ── Secondary-CPU park loop (boot): wfi + poll the PSCI cmd slot ────
    // The kernel's CPU_ON writes (CMD_MAGIC, entry) into this CPU's slot
    // and sends an SGI to wake the WFI; the loop then erets the CPU to EL1
    // at `entry` with the kernel's secondary boot path (SPSR = EL1h).
    .global monitor_park_loop
monitor_park_loop:
    wfi
    mrs  x10, MPIDR_EL1
    and  x10, x10, #0xff
    adrp x11, __psci_cmd_slots
    add  x11, x11, :lo12:__psci_cmd_slots
    movz x12, #16
    madd x11, x10, x12, x11     // &slot[cpu]
    ldr  x13, [x11, #0]
    movz x14, #0x4454           // CMD_MAGIC = 0x434D_4454
    movk x14, #0x434D, lsl #16
    cmp  x13, x14
    b.ne monitor_park_loop
    str  xzr, [x11, #0]         // clear the magic
    ldr  x12, [x11, #8]         // entry
    mov  x1, #0x501             // SCR_EL3 without... see above (NS|HCE|RW)
    msr  SCR_EL3, x1
    msr  ELR_EL3, x12
    mov  x13, #0x3c5            // SPSR_EL3: EL1h, DAIF masked
    msr  SPSR_EL3, x13
    isb
    eret
