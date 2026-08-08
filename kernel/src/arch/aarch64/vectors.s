// AArch64 exception vector table — Phase 6.
//
// Each slot is 128 bytes (32 instructions).  The table is 2 KiB-aligned.
//
// Improvements over Phase 3:
//   • Current-EL SPx IRQ (slot 5) → calls irq_handler instead of panicking.
//   • Lower-EL AArch64 Synchronous (slot 8) → dispatches SVC64 (EC 0x15,
//     Phase 6 EL0 servers) in the fast path via tanix_syscall, HVC and
//     aborts via sync_handler; only truly unexpected exceptions panic.
//   • Lower-EL AArch64 IRQ (slot 9) → calls irq_handler.
//   • All other slots still call exception_handler (which panics) — they
//     represent configurations we don't expect and should not ignore.
//
// Calling conventions (all handlers are extern "C"):
//   exception_handler(kind: u64, esr: u64, elr: u64, far: u64) -> !
//   irq_handler()                                                -> ()
//   sync_handler(esr: u64, elr: u64, far: u64, sp: u64)        -> ()
//   tanix_syscall(nr: u64, a0: u64, a1: u64, a2: u64)         -> u64

.section .text.vectors, "ax"
.align 11

.global __vectors
__vectors:

// ── Macro: full save + call exception_handler(kind, esr, elr, far) → panic ──

.macro EXCEPTION_ENTRY kind
    sub  sp, sp, #32
    stp  x0, x1, [sp, #0]
    stp  x2, x3, [sp, #16]
    mrs  x1, ESR_EL1
    mrs  x2, ELR_EL1
    mrs  x3, FAR_EL1
    mov  x0, #\kind
    bl   exception_handler
1:  b    1b
.endm

// ── Macro: save caller-saves, call irq_handler(), restore, return ────────────

.macro IRQ_ENTRY
    // Save all caller-saved registers (x0-x18, x29, x30, SP alignment).
    sub  sp, sp, #176
    stp  x0,  x1,  [sp,   #0]
    stp  x2,  x3,  [sp,  #16]
    stp  x4,  x5,  [sp,  #32]
    stp  x6,  x7,  [sp,  #48]
    stp  x8,  x9,  [sp,  #64]
    stp  x10, x11, [sp,  #80]
    stp  x12, x13, [sp,  #96]
    stp  x14, x15, [sp, #112]
    stp  x16, x17, [sp, #128]
    stp  x18, x30, [sp, #144]
    mrs  x0,  ELR_EL1
    mrs  x1,  SPSR_EL1
    stp  x0,  x1,  [sp, #160]

    bl   irq_handler

    // Restore and return.
    ldp  x0,  x1,  [sp, #160]
    msr  ELR_EL1,  x0
    msr  SPSR_EL1, x1
    ldp  x0,  x1,  [sp,   #0]
    ldp  x2,  x3,  [sp,  #16]
    ldp  x4,  x5,  [sp,  #32]
    ldp  x6,  x7,  [sp,  #48]
    ldp  x8,  x9,  [sp,  #64]
    ldp  x10, x11, [sp,  #80]
    ldp  x12, x13, [sp,  #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x30, [sp, #144]
    add  sp, sp, #176
    eret
.endm

// ── Macro: lower-EL synchronous → SVC fast path or sync_handler(esr, elr, far, sp) ──

.macro LOWER_SYNC_ENTRY
    sub  sp, sp, #176
    stp  x0,  x1,  [sp,   #0]
    stp  x2,  x3,  [sp,  #16]
    stp  x4,  x5,  [sp,  #32]
    stp  x6,  x7,  [sp,  #48]
    stp  x8,  x9,  [sp,  #64]
    stp  x10, x11, [sp,  #80]
    stp  x12, x13, [sp,  #96]
    stp  x14, x15, [sp, #112]
    stp  x16, x17, [sp, #128]
    stp  x18, x30, [sp, #144]
    mrs  x4,  ELR_EL1
    mrs  x5,  SPSR_EL1
    stp  x4,  x5,  [sp, #160]

    // ── Phase 6 fast path: SVC64 (EC 0x15) from EL0 → tanix_syscall ──────
    // x0 = syscall number, x1-x3 = arguments, result back in x0.
    // The kernel clobbers x1-x18 freely (the server-side wrapper declares
    // them clobbered); x30 is preserved below because the wrapper's `svc`
    // does not list it.
    mrs  x4, ESR_EL1
    lsr  x5, x4, #26
    cmp  x5, #0x15
    b.ne 1f
    bl   tanix_syscall         // result in x0
    // Restore everything except x0 (the result).  ELR_EL1/SPSR_EL1 must be
    // reloaded from the frame: the hardware already sets ELR to the address
    // after the `svc` (eret resumes at the wrapper's epilogue), and blocking
    // syscalls that context-switched leave ELR/SPSR pointing at the EL1h
    // resume point.
    ldp  x4,  x5,  [sp, #160]
    msr  ELR_EL1,  x4
    msr  SPSR_EL1, x5
    ldp  x2,  x3,  [sp,  #16]
    ldp  x4,  x5,  [sp,  #32]
    ldp  x6,  x7,  [sp,  #48]
    ldp  x8,  x9,  [sp,  #64]
    ldp  x10, x11, [sp,  #80]
    ldp  x12, x13, [sp,  #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x30, [sp, #144]
    add  sp, sp, #176
    eret
1:
    // Arguments: esr, elr, far, kernel_sp
    mrs  x0, ESR_EL1
    mrs  x1, ELR_EL1
    mrs  x2, FAR_EL1
    mov  x3, sp
    bl   sync_handler

    // Restore (sync_handler may have modified ELR to advance past HVC).
    ldp  x4,  x5,  [sp, #160]
    msr  ELR_EL1,  x4
    msr  SPSR_EL1, x5
    ldp  x0,  x1,  [sp,   #0]
    ldp  x2,  x3,  [sp,  #16]
    ldp  x4,  x5,  [sp,  #32]
    ldp  x6,  x7,  [sp,  #48]
    ldp  x8,  x9,  [sp,  #64]
    ldp  x10, x11, [sp,  #80]
    ldp  x12, x13, [sp,  #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x30, [sp, #144]
    add  sp, sp, #176
    eret
.endm

// ── Vector table entries ──────────────────────────────────────────────────────

// 0: Current EL / SP0 — Synchronous
.balign 128
    EXCEPTION_ENTRY 0

// 1: Current EL / SP0 — IRQ
.balign 128
    EXCEPTION_ENTRY 1

// 2: Current EL / SP0 — FIQ
.balign 128
    EXCEPTION_ENTRY 2

// 3: Current EL / SP0 — SError
.balign 128
    EXCEPTION_ENTRY 3

// 4: Current EL / SPx — Synchronous
.balign 128
    EXCEPTION_ENTRY 4

// 5: Current EL / SPx — IRQ  ← real IRQ handler
.balign 128
    IRQ_ENTRY

// 6: Current EL / SPx — FIQ
.balign 128
    EXCEPTION_ENTRY 6

// 7: Current EL / SPx — SError
.balign 128
    EXCEPTION_ENTRY 7

// 8: Lower EL (AArch64) — Synchronous  ← SVC64 fast path + HVC/abort dispatch
.balign 128
    LOWER_SYNC_ENTRY

// 9: Lower EL (AArch64) — IRQ  ← real IRQ handler
.balign 128
    IRQ_ENTRY

// 10: Lower EL (AArch64) — FIQ
.balign 128
    EXCEPTION_ENTRY 10

// 11: Lower EL (AArch64) — SError
.balign 128
    EXCEPTION_ENTRY 11

// 12: Lower EL (AArch32) — Synchronous
.balign 128
    EXCEPTION_ENTRY 12

// 13: Lower EL (AArch32) — IRQ
.balign 128
    EXCEPTION_ENTRY 13

// 14: Lower EL (AArch32) — FIQ
.balign 128
    EXCEPTION_ENTRY 14

// 15: Lower EL (AArch32) — SError
.balign 128
    EXCEPTION_ENTRY 15
