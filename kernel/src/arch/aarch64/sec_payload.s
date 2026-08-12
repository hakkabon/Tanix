// Phase 16 — secure-world payload (S-EL1), the TrustZone "secure core".
//
// The whole region is ONE position-independent blob:
//   +0x0000  secure vector table  (VBAR_EL1 = region base; FIQ at 0x300)
//   +0x0800  sec_entry — entry point (first world switch)
//   +0x6000  sec_data — shared data (monitor writes iters/round/uart,
//                       payload counts ticks; monitor serves read them)
//   +0x7000  secure stack (grows down, 4 KiB)
//   +0x8000  end of region (32 KiB total)
//
// The EL3 monitor copies the blob into secure RAM on `sbsa-ref`
// (0x20000000, NS-invisible) and runs it in place on `virt`.  Everything
// must therefore be pc-relative: no absolute literals, no relocations.
// All addresses use `adr`/`adrp` against symbols inside the region, so the
// copy delta cancels out.  The only external inputs are written into
// sec_data by the monitor (secure UART base, iteration count, round).
//
// Flow: sec_entry arms the secure EL1 physical timer (CNTPS_EL1, PPI 29
// Group 0 FIQ) and loops `iters` times, counting ticks.  Each tick is
// counted exactly once: the FIQ handler (TrustZone secure IRQ path) and
// the poll fallback both go through sec_tick_increment, which acks the
// interrupt before counting, so the two paths can never double-count.
// After `iters` iterations the payload exits to the monitor with
// `smc #0`; the tick count travels back to the non-secure kernel.

    .section .sec_payload, "ax", %progbits
    .global sec_payload_start
sec_payload_start:

    // ── Secure vector table (2 KiB aligned by the linker; VBAR = base) ──
    // Slots: 0x000 sync/sp0, 0x080 irq/sp0, 0x100 fiq/sp0, 0x180 serr/sp0,
    //        0x200 sync/spx, 0x280 irq/spx, 0x300 fiq/spx, 0x380 serr/spx,
    //        0x400+ lower EL (A64/A32).
    .org 0x000
    b   sec_default
    .org 0x080
    b   sec_default
    .org 0x100
    b   sec_default
    .org 0x180
    b   sec_default
    .org 0x200
    b   sec_default
    .org 0x280
    b   sec_default
    .org 0x300
    b   sec_fiq                 // current EL / SPx FIQ → secure timer
    .org 0x380
    b   sec_default
    .org 0x400
    b   sec_default
    .org 0x480
    b   sec_default
    .org 0x500
    b   sec_default
    .org 0x580
    b   sec_default
    .org 0x600
    b   sec_default
    .org 0x680
    b   sec_default
    .org 0x700
    b   sec_default
    .org 0x780
    b   sec_default

    // ── Entry (first world switch) ───────────────────────────────────────
    // Params are read from sec_data (written by the monitor), never from
    // registers: the payload is fully self-contained once copied.
    .org 0x800
    .global sec_entry
sec_entry:
    adr  x4, sec_data_iters
    ldr  x5, [x4, #0]          // iters
    ldr  x6, [x4, #8]          // round
    ldr  x7, [x4, #16]         // secure UART base
    // banner: "sec: round <round> armed\n"
    mov  x11, x7
    adr  x12, sec_msg_round
    bl   sec_uart_puts
    mov  x0, x6
    bl   sec_print_uint
    adr  x12, sec_msg_armed
    bl   sec_uart_puts
    // arm the secure timer: TVAL = CNTFRQ / 20 (50 ms period), enable.
    mrs  x9, CNTFRQ_EL0
    movz x10, #20
    udiv x9, x9, x10
    msr  CNTPS_TVAL_EL1, x9
    mov  x9, #1
    msr  CNTPS_CTL_EL1, x9
    msr  ICC_IGRPEN0_EL1, x9   // enable Group 0 (FIQ) delivery at S-EL1
    isb
    // main loop: `iters` iterations of poll-the-timer.
sec_loop:
    subs x5, x5, #1
    b.eq sec_done
    msr  daifset, #1           // mask FIQ around the poll path
    mrs  x9, CNTPS_CTL_EL1
    tst  x9, #4                // ISTATUS
    b.eq 9f
    bl   sec_tick_increment
9:  msr  daifclr, #1
    b    sec_loop
sec_done:
    adr  x4, sec_data_ticks
    ldr  x0, [x4]              // return the tick count to the kernel
    smc  #0

    // ── Secure timer FIQ handler (Group 0, routed to S-EL1) ──────────────
sec_fiq:
    sub  sp, sp, #96
    stp  x9,  x10, [sp, #0]
    stp  x11, x12, [sp, #16]
    stp  x13, x14, [sp, #32]
    stp  x15, x16, [sp, #48]
    stp  x17, x18, [sp, #64]
    stp  x30, xzr, [sp, #80]
    bl   sec_tick_increment
    ldp  x9,  x10, [sp, #0]
    ldp  x11, x12, [sp, #16]
    ldp  x13, x14, [sp, #32]
    ldp  x15, x16, [sp, #48]
    ldp  x17, x18, [sp, #64]
    ldp  x30, xzr, [sp, #80]
    add  sp, sp, #96
    eret

    // ── Shared tick handling: count, ack, rearm, periodic print ──────────
    // Clobbers x9..x18, x30.  Used by the FIQ handler and the poll path;
    // the ack (IAR0/EOIR0) before counting makes double-counting
    // impossible even if the FIQ fires between the poll's ISTATUS check
    // and here (the poll masks FIQ around the check).
sec_tick_increment:
    sub  sp, sp, #32
    stp  x30, x9,  [sp, #0]
    stp  x10, x11, [sp, #16]
    adr  x9, sec_data_ticks
    ldr  x10, [x9, #0]
    add  x10, x10, #1
    str  x10, [x9, #0]
    mrs  x11, ICC_IAR0_EL1     // ack Group 0
    msr  ICC_EOIR0_EL1, x11
    mrs  x11, CNTFRQ_EL0       // rearm: TVAL = CNTFRQ / 20
    movz x12, #20
    udiv x11, x11, x12
    msr  CNTPS_TVAL_EL1, x11
    // print every 50th tick on the secure console
    movz x11, #50
    udiv x12, x10, x11
    msub x12, x12, x11, x10    // x12 = ticks % 50
    cbnz x12, 9f
    adr  x11, sec_data_uart
    ldr  x11, [x11]            // secure UART base
    adr  x12, sec_msg_tick
    bl   sec_uart_puts
    mov  x0, x10
    bl   sec_print_uint
    adr  x12, sec_msg_nl
    bl   sec_uart_puts
9:  ldp  x30, x9,  [sp, #0]
    ldp  x10, x11, [sp, #16]
    add  sp, sp, #32
    ret

    // ── PL011 putc / puts (polling, secure UART) ─────────────────────────
    // sec_uart_puts: x11 = uart base, x12 = string ptr.  Clobbers x9..x12.
sec_uart_puts:
    sub  sp, sp, #32
    stp  x30, x9,  [sp, #0]
    stp  x10, x11, [sp, #16]
1:  ldrb w9, [x12], #1
    cbz  w9, 9f
    bl   sec_uart_putc
    b    1b
9:  ldp  x30, x9,  [sp, #0]
    ldp  x10, x11, [sp, #16]
    add  sp, sp, #32
    ret

    // sec_uart_putc: x11 = uart base, w9 = char.  Clobbers x10.
sec_uart_putc:
1:  ldr  w10, [x11, #0x18]     // PL011_FR — TXFF = bit 5
    tbnz w10, #5, 1b
    str  w9, [x11, #0]         // PL011_DR
    ret

    // ── Decimal print: x0 = value, x11 = uart base.  Clobbers x9..x18. ──
sec_print_uint:
    sub  sp, sp, #32
    stp  x30, x9,  [sp, #0]
    stp  x10, x11, [sp, #16]   // x11 = uart base (preserved)
    adr  x9, sec_data_scratch
    add  x10, x9, #24          // end of the 24-byte scratch buffer
    mov  x12, x10
2:  movz x13, #10
    udiv x14, x0, x13
    msub x15, x14, x13, x0     // x15 = value % 10
    add  x15, x15, #'0'
    sub  x12, x12, #1
    strb w15, [x12]
    mov  x0, x14
    cbnz x14, 2b
3:  ldrb w9, [x12], #1
    cbz  w9, 9f
    bl   sec_uart_putc
    b    3b
9:  ldp  x30, x9,  [sp, #0]
    ldp  x10, x11, [sp, #16]
    add  sp, sp, #32
    ret

    // ── Unexpected exception in the secure world ─────────────────────────
sec_default:
    wfi
    b    sec_default

    // ── Strings (in the code area; pc-relative access) ───────────────────
sec_msg_round: .asciz "sec: round "
sec_msg_armed: .asciz " armed — secure world alive\n"
sec_msg_tick:  .asciz "sec: tick "
sec_msg_nl:    .asciz "\n"

    // ── Shared data (monitor ↔ payload ↔ services) ───────────────────────
    .org 0x6000
    .global sec_data_iters
sec_data_iters:   .quad 0      // +0x00  iterations per world switch
    .global sec_data_round
sec_data_round:   .quad 0      // +0x08  round number (banner)
    .global sec_data_uart
sec_data_uart:    .quad 0      // +0x10  secure UART base (monitor-written)
    .global sec_data_ticks
sec_data_ticks:   .quad 0      // +0x18  secure tick count (payload-owned)
    .global sec_data_counter
sec_data_counter: .quad 0      // +0x20  monotonic counter (monitor service)
    .global sec_data_measure
sec_data_measure: .quad 0      // +0x28  TCB measurement digest (monitor)
    .global sec_data_scratch
sec_data_scratch: .space 24    // +0x30  itoa scratch

    // ── Secure stack (4 KiB, grows down) ─────────────────────────────────
    .org 0x7000
    .global sec_stack_top
sec_stack_top:

    .org 0x8000
    .global sec_payload_end
sec_payload_end:
    .p2align 15                 // advertise 32 KiB alignment for the region
