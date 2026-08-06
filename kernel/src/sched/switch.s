// AArch64 context switch stub.
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
//    88     lr  (x30)
//    96     sp
//  (total = 13 × 8 = 104 bytes)

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
    stp  x29, x30, [x0, #80]   // fp, lr
    mov  x9,  sp
    str  x9,       [x0, #96]   // sp

    // ── Restore next task (x1 = *to) ─────────────────────────────────────
    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]   // fp, lr
    ldr  x9,       [x1, #96]   // sp
    mov  sp,  x9

    // Return into the restored lr (= next task's entry point or resume PC).
    ret
