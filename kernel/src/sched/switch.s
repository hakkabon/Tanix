// AArch64 context switch stub — Phase 6 (EL0 servers + per-task address
// spaces).
//
// Signature (C ABI):
//   void context_switch(Context *from, const Context *to);
//
// Context layout (must match sched::task::Context in task.rs):
//   Offset  Field
//     0     x19
//     8     x20
//    16     x21
//    24     x22
//    32     x23
//    40     x24
//    48     x25
//    56     x26
//    64     x27
//    72     x28
//    80     fp  (x29)
//    88     lr  (x30)   — resume PC, loaded into ELR_EL1 before `eret`
//    96     sp          — kernel stack pointer (SP_EL1)
//   104     sp_el0      — user stack pointer (EL0 tasks only)
//   112     spsr        — SPSR_EL1 (0x3C5 EL1h / 0x3C0 EL0t)
//   120     ttbr0       — task page table (kernel table for kernel tasks)
//  (total = 16 × 8 = 128 bytes)
//
// Notes:
//   • The restore side ends in `eret`: it returns into the next task at the
//     exception level encoded in its saved SPSR (EL1h for kernel contexts
//     and the Phase-3 guest, EL0t for Phase-6 servers).
//   • The save side stores a constant SPSR (EL1h) instead of reading
//     SPSR_EL1: when a context is saved while running at EL1, SPSR_EL1
//     still holds the PSTATE of the last *exception return* (e.g. an EL0
//     task's SVC), not the current EL1 PSTATE.  Resuming a kernel context
//     must always return to EL1h, so the constant is always correct.
//   • TTBR0_EL1 is switched per task and the TLB fully invalidated, since
//     we use no ASIDs.  This is safe here: between the `msr` and the `isb`
//     the stub executes only kernel code, which every table maps EL1-only.
//   • Interrupts never fire during a switch (no interrupt source is
//     enabled and DAIF stays masked), so there is no need to mask IRQs.

.section .text, "ax"
.global context_switch
.type context_switch, %function

context_switch:
    // ── Save current task (x0 = *from) ───────────────────────────────────
    stp  x19, x20, [x0, #0]
    stp  x21, x22, [x0, #16]
    stp  x23, x24, [x0, #32]
    stp  x25, x26, [x0, #48]
    stp  x27, x28, [x0, #64]
    stp  x29, x30, [x0, #80]   // fp, lr (resume PC)
    mov  x9,  sp
    str  x9,       [x0, #96]   // kernel sp
    mrs  x9,  SP_EL0
    str  x9,       [x0, #104]  // user sp_el0 (preserved across EL1 code)
    mov  x9,  #0x3c5           // SPSR_EL1: EL1h, DAIF masked
    str  x9,       [x0, #112]  // kernel contexts always resume at EL1h
    mrs  x9,  TTBR0_EL1
    str  x9,       [x0, #120]  // active page table

    // ── Restore next task (x1 = *to) ─────────────────────────────────────
    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]   // fp, lr (resume PC → ELR_EL1)
    ldr  x9,       [x1, #96]   // kernel sp
    mov  sp,  x9
    ldr  x9,       [x1, #104]  // user sp_el0
    msr  SP_EL0,   x9
    ldr  x9,       [x1, #112]  // SPSR_EL1
    msr  SPSR_EL1, x9
    ldr  x9,       [x1, #120]  // page table
    msr  TTBR0_EL1, x9
    isb
    tlbi vmalle1is
    dsb  sy
    isb
    msr  ELR_EL1, x30
    eret
