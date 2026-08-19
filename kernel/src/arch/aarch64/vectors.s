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

// ── Macro: save, call irq_handler(from_el0, frame), restore, return ────────
//
// `\lower` is 0 for current-EL IRQs (slot 5 — landed inside the kernel or
// inside an EL1 guest) and 1 for lower-EL IRQs (slot 9 — an EL0 task was
// interrupted).  The scheduler only preempts on the latter — and, since
// Phase 21, on slot-5 ticks that land inside a guest vCPU, which is why
// this macro (unlike the slot-8/9 SMALLER frames) also saves the
// callee-saved registers x19-x29.
//
// Frame layout (272 bytes, in ::vm::sched and `restore_preempted_guest`):
//   [sp+0]   x0    [sp+80]  x10   [sp+160] ELR_EL1  [sp+240] x27
//   [sp+8]   x1    [sp+88]  x11   [sp+168] SPSR_EL1 [sp+248] x28
//   [sp+16]  x2    [sp+96]  x12   [sp+176] x19      [sp+256] x29 (fp)
//   [sp+24]  x3    [sp+104] x13   [sp+184] x20      [sp+264] pad (SP align)
//   [sp+32]  x4    [sp+112] x14   [sp+192] x21
//   [sp+40]  x5    [sp+120] x15   [sp+200] x22
//   [sp+48]  x6    [sp+128] x16   [sp+208] x23
//   [sp+56]  x7    [sp+136] x17   [sp+216] x24
//   [sp+64]  x8    [sp+144] x18   [sp+224] x25
//   [sp+72]  x9    [sp+152] x30   [sp+232] x26
//
// The complete per-vCPU snapshot ([sp+0..168] + [sp+176..264], ELR, SPSR)
// is what the Phase-21 tenant preemption captures into a preempted guest's
// context: `restore_preempted_guest` reloads everything from this frame
// and `eret`s back into the interrupted guest.

.macro IRQ_ENTRY lower
    // Save the full register set: caller-saved (x0-x18, x30), ELR/SPSR,
    // and — Phase 21 — callee-saved (x19-x29) so a tick inside a guest
    // vCPU can capture the whole preemption point.
    sub  sp, sp, #272
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
    stp  x19, x20, [sp, #176]
    stp  x21, x22, [sp, #192]
    stp  x23, x24, [sp, #208]
    stp  x25, x26, [sp, #224]
    stp  x27, x28, [sp, #240]
    str  x29,      [sp, #256]

    mov  x0, #\lower
    mov  x1, sp        // frame base — Phase 21 guest-preemption capture
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
    ldp  x19, x20, [sp, #176]
    ldp  x21, x22, [sp, #192]
    ldp  x23, x24, [sp, #208]
    ldp  x25, x26, [sp, #224]
    ldp  x27, x28, [sp, #240]
    ldr  x29,      [sp, #256]
    add  sp, sp, #272
    eret
.endm

// ── Slot 8: lower-EL synchronous → SVC fast path or sync_handler(esr, elr, far, sp) ──
//
// IMPORTANT: each vector slot is exactly 128 bytes.  The full handler
// (full register frame + SVC64 dispatch) does not fit, so the slot is a
// branch stub and the real code lives OUT-OF-LINE (`lower_sync_full`).
// Inlining it used to overflow the slot, and the `.balign 128` shifted
// every following entry down by one: slot 9 (IRQ) then contained the tail
// of this handler (an `eret`), so IRQs from EL0 were silently dismissed
// instead of being handled.

.macro LOWER_SYNC_ENTRY
    b   lower_sync_full
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

// 5: Current EL / SPx — IRQ  ← IRQ landed inside the kernel (wait loop)
.balign 128
    IRQ_ENTRY 0

// 6: Current EL / SPx — FIQ
.balign 128
    EXCEPTION_ENTRY 6

// 7: Current EL / SPx — SError
.balign 128
    EXCEPTION_ENTRY 7

// 8: Lower EL (AArch64) — Synchronous  ← SVC64 fast path + HVC/abort dispatch
.balign 128
    LOWER_SYNC_ENTRY

// 9: Lower EL (AArch64) — IRQ  ← EL0 task interrupted; preemption point
.balign 128
    IRQ_ENTRY 1

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

// ── Out-of-line: slot 8 stub target (lower-EL synchronous dispatch) ──────────
//    Kept OUTSIDE the vector table: the full body with its SVC64 fast path
//    and non-SVC fallback is larger than a 128-byte slot.  Inlining it used
//    to overflow slot 8, and the `.balign 128` shifted every following entry
//    down by one — slot 9 (IRQ) then contained this handler's trailing
//    `eret`, so IRQs from EL0 were silently dismissed instead of handled.

lower_sync_full:
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
    b.ne lower_sync_fallback
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

// ── Phase 21: preempted-guest resume stub ─────────────────────────────────────
//
// A tick that preempts an EL1 guest captures the whole IRQ_ENTRY frame
// (see the frame layout above) into the guest's context and hands the CPU
// to another tenant.  When the preempted tenant is later re-run,
// `context_switch` restores its context with:
//   sp = frame base (the guest's IRQ frame top),  ELR = this stub,
//   SPSR = EL1h (masked),  x19-x29 = the guest's callee-saved values
// (context_switch restored them from the context; this stub reloads them
// from the frame copy for a single source of truth).
//
// The stub then resurrects the guest from the frame: caller-saved
// registers, ELR_EL1/SPSR_EL1 (the guest's real PSTATE, IRQ unmasked),
// and finally pops the frame before `eret` so SP is exactly where the
// guest left it.  Nothing on this path may touch the stack before the
// final `add sp, sp, #272` — the frame itself is the working area.
//
// Interrupts are masked until the final `eret` (SPSR_EL1 is only applied
// at exception return), so the 272-byte reload cannot itself be
// interrupted.

.section .text, "ax"
.global restore_preempted_guest
.type restore_preempted_guest, %function
restore_preempted_guest:
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
    ldp  x19, x20, [sp, #176]
    ldp  x21, x22, [sp, #192]
    ldp  x23, x24, [sp, #208]
    ldp  x25, x26, [sp, #224]
    ldp  x27, x28, [sp, #240]
    ldr  x29,      [sp, #256]
    ldp  x9,  x10, [sp, #160]
    msr  ELR_EL1,  x9
    msr  SPSR_EL1, x10
    add  sp, sp, #272
    eret

// ── Phase 21: yield entry — interrupt-masked prologue ─────────────────────────
//
// The guest calls `vm_yield_entry` to hand control back to the kernel.  The
// cooperative switch inside must never be interrupted: a preemption tick in
// the middle of `context_switch` would capture a half-switched register
// file.  This tiny prologue masks IRQ (they are already masked whenever the
// kernel runs; only guest execution is unmasked) and then falls through
// into the Rust implementation.  Guests are entered with IRQ enabled
// (SPSR 0x345), so unless masked here the tick could land mid-switch.
// #2 in `daifset` = the I bit.

.section .text, "ax"
.global vm_yield_entry
.type vm_yield_entry, %function
vm_yield_entry:
    msr  daifset, #2
    isb
    b    vm_yield_entry_masked

// ── Out-of-line fallback: lower-EL synchronous exceptions that are not
//    SVC64 (HVC, aborts). ──────────────────────────────────────────────────

lower_sync_fallback:
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
