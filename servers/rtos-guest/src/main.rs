//! Zephyr-style RTOS guest — Phase 21 co-tenant VM.
//!
//! A real guest OS, modelled on Zephyr's kernel objects, sharing the
//! physical CPU with other tenants under the Tanix EL1 VMM:
//!
//!   • k_threads        — priority-scheduled threads with cooperative
//!                        switching (the EL1 physical timer belongs to the
//!                        VMM, so the guest itself has no preemptive clock;
//!                        the *kernel* preempts the whole tenant on its
//!                        tick and resumes it exactly where it stopped).
//!   • k_sem            — counting semaphores with waiter queues.
//!   • k_msgq           — bounded message queues with get/put waiters.
//!   • k_sleep / k_timer— monotonic deadlines driven by CNTVCT_EL0.  When
//!                        every thread sleeps, the idle thread advances
//!                        time by busy-waiting to the earliest deadline
//!                        (the vCPU's tickless idle); the kernel
//!                        time-slices the guest meanwhile.
//!
//! Boot protocol (identical to the Phase-3 stub):
//!   x4 — shared-memory physical base (VirtqueueConfig + VMM info block)
//!   x5 — kernel's `vm_yield_entry` (yield = "pause this tenant")
//!   x6 — guest-context pointer the kernel uses to resume us
//!   x7 — machine console (PL011) base: 0x6000_0000 on sbsa-ref,
//!        0x0900_0000 on virt
//!
//! Guest ↔ kernel contract (info block at shmem + 0x3000):
//!   magic   (u32, "IVMM")        ← kernel publishes
//!   tenant  (u32 at +4)          ← kernel publishes (print prefix)
//!   state   (u32 at +0x10)       ← guest publishes: RUNNING / PARKED
//!
//! Demo: producer/consumer exchange messages through k_msgq, rendezvous
//! through k_sem, the timer thread counts k_timer-style rounds, and the
//! idle thread drains the kernel's VirtIO ring (Print → Echo) whenever the
//! RTOS has nothing else to do.  When every thread exits, the guest
//! publishes PARKED and yields — the kernel stops scheduling it.
//!
//! Platform: what the kernel tells us via boot x7 — sbsa-ref NS PL011
//! (0x6000_0000) or virt (0x0900_0000).  A real upstream Zephyr
//! build would additionally require EL2 stage-2 isolation and its own
//! GIC/timer view — outside this VM model; see the kernel README.

#![no_std]
#![no_main]

use core::arch::global_asm;
use core::panic::PanicInfo;

// ── UART (PL011; base from boot x7, kernel-published) ─────────────────────────

static mut UART_BASE: usize = 0x6000_0000;

fn set_uart_base(uart: u64) {
    if uart != 0 {
        unsafe { UART_BASE = uart as usize; }
    }
}

fn uart_dr() -> *mut u32 {
    unsafe { (UART_BASE + 0x0000) as *mut u32 }
}
fn uart_fr() -> *const u32 {
    unsafe { (UART_BASE + 0x0018) as *const u32 }
}
const FR_TXFF: u32 = 1 << 5;

fn putc(b: u8) {
    unsafe {
        while core::ptr::read_volatile(uart_fr()) & FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(uart_dr(), b as u32);
    }
}

fn puts(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

fn put_u32(v: u32) {
    let d = [
        (v / 1000) % 10, (v / 100) % 10, (v / 10) % 10, v % 10,
    ];
    for x in d {
        putc(b'0' + x as u8);
    }
}

fn put_u32_hex(v: u32) {
    let nybbles = [
        (v >> 28) & 0xF, (v >> 24) & 0xF, (v >> 20) & 0xF, (v >> 16) & 0xF,
        (v >> 12) & 0xF, (v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF,
    ];
    puts("0x");
    for n in nybbles {
        putc(if n < 10 { b'0' + n as u8 } else { b'a' + (n - 10) as u8 });
    }
}

fn put_u64_hex(v: u64) {
    let nybbles = [
        (v >> 60) & 0xF, (v >> 56) & 0xF, (v >> 52) & 0xF, (v >> 48) & 0xF,
        (v >> 44) & 0xF, (v >> 40) & 0xF, (v >> 36) & 0xF, (v >> 32) & 0xF,
    ];
    puts("0x");
    for n in nybbles {
        putc(if n < 10 { b'0' + n as u8 } else { b'a' + (n - 10) as u8 });
    }
    put_u32_hex(v as u32);
}

/// Register dump used by the panic handler.  Reads every register with
/// `mov`/`mrs` only (no stack, no memory) into a static, then prints them.
/// The values are the register file AT THE PANIC, before any handler
/// prologue can trample caller-saved registers.
#[allow(dead_code)]
static mut PANIC_REGS: [u64; 25] = [0; 25];

fn raw_reg_dump() {
let regs = unsafe { PANIC_REGS.as_mut_ptr() };
    unsafe {
        core::arch::asm!(
            // Slots: [0]SPSR [1]ELR [2]ESR [3..21]x0..x18 [22]x30
            //        [23]sp [24]x29.  x13 holds the buffer across the
            //        dump (inout); x9 is scratch (declared out so the
            //        compiler keeps its own values elsewhere).
            "stp x0,  x1,  [x13, #24]",
            "stp x2,  x3,  [x13, #40]",
            "stp x4,  x5,  [x13, #56]",
            "stp x6,  x7,  [x13, #72]",
            "stp x8,  x9,  [x13, #88]",
            "stp x10, x11, [x13, #104]",
            "stp x12, x13, [x13, #120]",
            "stp x14, x15, [x13, #136]",
            "stp x16, x17, [x13, #152]",
            "stp x18, x30, [x13, #168]",
            "mov x9, sp",
            "str x9, [x13, #184]",
            "mov x9, x29",
            "str x9, [x13, #192]",
            "mrs x9, SPSR_EL1",
            "str x9, [x13, #0]",
            "mrs x9, ESR_EL1",
            "str x9, [x13, #16]",
            "mrs x9, ELR_EL1",
            "str x9, [x13, #8]",
            inout("x13") regs => _,
            out("x9") _,
        );
    }
    for i in 0..25 {
        puts("\n gR");
        put_u64_hex(unsafe { core::ptr::read_volatile(regs.add(i)) });
    }
    puts("\n");
}

// ── Guest time base ──────────────────────────────────────────────────────────
// Generic timer at EL1: frequency from CNTFRQ_EL0, counter from CNTVCT_EL0
// (no EL2 virtualization here, so virtual time == physical time).

fn cntfrq() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, CNTFRQ_EL0", out(reg) f, options(nomem, nostack)) }
    if f == 0 { 1_000_000 } else { f }
}

fn cntvct() -> u64 {
    let t: u64;
    unsafe { core::arch::asm!("mrs {}, CNTVCT_EL0", out(reg) t, options(nomem, nostack)) }
    t
}

fn ticks_per_ms() -> u64 {
    cntfrq() / 1000
}

// ── Cooperative yield to the kernel tenant scheduler ─────────────────────────

/// The kernel's `vm_yield_entry(guest_ctx: usize)` — pauses this tenant.
/// Returns when the kernel schedules us again.
type YieldFn = unsafe extern "C" fn(guest_ctx: usize);

// ── RTOS kernel objects ──────────────────────────────────────────────────────

const MAX_THREADS: usize = 8;
const MAX_WAITERS: usize = 8;

/// Thread states.
const TS_READY: u8 = 0;
const TS_SEM: u8 = 1;   // blocked on a semaphore
const TS_GET: u8 = 2;   // blocked on a message-queue get
const TS_PUT: u8 = 3;   // blocked on a message-queue put
const TS_SLEEP: u8 = 4; // blocked until its deadline
const TS_EXIT: u8 = 5;

/// Saved register set of one guest thread (setjmp-style, cf. `gs_save`).
#[derive(Clone, Copy)]
#[repr(C)]
struct GuestJb {
    /// x19–x28.
    x19x28: [u64; 10],
    /// x29 (fp).
    fp: u64,
    /// x30 (lr) — resume PC; `gs_restore` ends in `ret`.
    lr: u64,
    /// Stack pointer.
    sp: u64,
}

const fn jb_zeroed() -> GuestJb {
    GuestJb { x19x28: [0; 10], fp: 0, lr: 0, sp: 0 }
}

#[derive(Clone, Copy)]
struct Tcb {
    state: u8,
    prio: u8,
    jb: GuestJb,
    stack_top: usize,
    /// Object this thread is blocked on (-2 = sleeper, -1 = none, else
    /// semaphore id).
    wait: i32,
    deadline: u64,
    rounds: u32,
}

const fn tcb_zeroed() -> Tcb {
    Tcb {
        state: TS_EXIT,
        prio: 255,
        jb: jb_zeroed(),
        stack_top: 0,
        wait: -1,
        deadline: 0,
        rounds: 0,
    }
}

/// Counting semaphore with a fixed waiter list.
#[derive(Clone, Copy)]
struct Sem {
    count: i32,
    waiters: [i32; MAX_WAITERS],
    n_waiters: usize,
}

const fn sem_zeroed() -> Sem {
    Sem { count: 0, waiters: [0; MAX_WAITERS], n_waiters: 0 }
}

/// Bounded message queue (Zephyr k_msgq model).
#[derive(Clone, Copy)]
struct Msgq {
    buf: [u8; 64],
    size: usize, // per-message bytes
    depth: usize,
    head: usize,
    count: usize,
    get_waiters: [i32; MAX_WAITERS],
    n_get: usize,
    put_waiters: [i32; MAX_WAITERS],
    n_put: usize,
}

const fn msgq_zeroed() -> Msgq {
    Msgq {
        buf: [0; 64],
        size: 0,
        depth: 0,
        head: 0,
        count: 0,
        get_waiters: [0; MAX_WAITERS],
        n_get: 0,
        put_waiters: [0; MAX_WAITERS],
        n_put: 0,
    }
}

// ── RTOS tables (single static, zeroed .bss; kernel pre-zeroes guest RAM) ───

static mut THREADS: [Tcb; MAX_THREADS] = [tcb_zeroed(); MAX_THREADS];
static mut CUR: i32 = -1; // current thread id; -1 = the idle thread
static mut IDLE_JB: GuestJb = jb_zeroed();
static mut SEM_A: Sem = sem_zeroed();
static mut SEM_B: Sem = sem_zeroed();
static mut MSGQ_DEMO: Msgq = msgq_zeroed();

/// Stack pool for guest threads (8 KiB each; guest RAM is 1 MiB).
static mut THREAD_STACKS: [u8; MAX_THREADS * 8192] = [0; MAX_THREADS * 8192];

// ── Thread switch — setjmp-style save/restore ────────────────────────────────
//
//   u64 gs_save(GuestJb *jb);       // returns 0 on save; 1 when resumed
//   void gs_restore(const GuestJb *jb); // never returns (resumes saved PC
//                                       // via `ret`)
//
// Only x19-x29 + sp + lr are saved; caller-saved registers live across the
// switch in the blocking call's own frame (like the kernel's
// `context_switch`).
//
// The kernel's tick can preempt this guest anywhere, including mid-switch:
// the VMM captures the complete register file from its IRQ frame and
// resumes byte-identical state, so the switch is transparent to it.  Only
// the yield-to-kernel boundary (vm_yield_entry) masks IRQs.

global_asm!(
    r#"
.section .text, "ax"
.global gs_save
.type gs_save, %function
gs_save:
    stp  x19, x20, [x0, #0]
    stp  x21, x22, [x0, #16]
    stp  x23, x24, [x0, #32]
    stp  x25, x26, [x0, #48]
    stp  x27, x28, [x0, #64]
    stp  x29, x30, [x0, #80]
    mov  x9, sp
    str  x9, [x0, #96]
    mov  x0, #1
    ret

.section .text, "ax"
.global gs_restore
.type gs_restore, %function
gs_restore:
    ldp  x19, x20, [x0, #0]
    ldp  x21, x22, [x0, #16]
    ldp  x23, x24, [x0, #32]
    ldp  x25, x26, [x0, #48]
    ldp  x27, x28, [x0, #64]
    ldp  x29, x30, [x0, #80]
    ldr  x9,  [x0, #96]
    mov  sp, x9
    ret
    "#
);

extern "C" {
    fn gs_save(jb: &mut GuestJb) -> u64;
    fn gs_restore(jb: *const GuestJb) -> !;
}

// ── Scheduler core ───────────────────────────────────────────────────────────

/// Pick the highest-priority READY thread (strictly better than the
/// current one), or -1 when nobody may run.
fn pick_ready() -> i32 {
    unsafe {
        let me = CUR;
        let mut best: i32 = -1;
        let mut best_prio: u8 = 255;
        for i in 0..MAX_THREADS {
            let t = &THREADS[i];
            if t.state == TS_READY && (i as i32) != me && t.prio < best_prio {
                best = i as i32;
                best_prio = t.prio;
            }
        }
        best
    }
}

/// Switch to the best ready thread, or back to the idle thread.  Never
/// returns.
fn dispatch() -> ! {
    let next = pick_ready();
    unsafe {
        if next >= 0 {
            CUR = next;
            gs_restore(&THREADS[next as usize].jb as *const GuestJb);
        } else {
            CUR = -1;
            gs_restore(&IDLE_JB as *const GuestJb);
        }
    }
}

/// Block the current thread on `state`/`wait`: save its registers, then
/// dispatch away.  When the thread is later woken and re-scheduled,
/// `gs_save` returns 1 and this function returns — the blocking call
/// re-checks its condition in a loop.
fn block(state: u8, wait: i32) {
    unsafe {
        let me = CUR as usize;
        THREADS[me].state = state;
        THREADS[me].wait = wait;
        let mut jb = THREADS[me].jb;
        let resumed = gs_save(&mut jb);
        THREADS[me].jb = jb;
        if resumed == 0 {
            dispatch(); // first pass: hand the CPU away
        }
    }
}

/// Move the oldest waiter of a list to READY.
unsafe fn wake_waiter(waiters: &mut [i32; MAX_WAITERS], n: &mut usize) {
    if *n > 0 {
        let who = waiters[0];
        waiters.copy_within(1..*n, 0);
        *n -= 1;
        THREADS[who as usize].state = TS_READY;
    }
}

fn cur_tid() -> usize {
    unsafe { CUR as usize }
}

/// k_yield: cooperative round-robin within the same/any-better priority.
fn k_yield() {
    let me = cur_tid();
    unsafe {
        let mut jb = THREADS[me].jb;
        let resumed = gs_save(&mut jb);
        THREADS[me].jb = jb;
        if resumed == 0 {
            let next = pick_ready();
            if next >= 0 {
                dispatch(); // never returns
            }
        }
    }
}

/// k_sleep(ms): block until a monotonic deadline; the idle thread advances
/// time (tickless idle) and re-readies this thread when the deadline
/// passes.
fn k_sleep(ms: u64) {
    let dl = cntvct() + ms.saturating_mul(ticks_per_ms());
    unsafe {
        THREADS[cur_tid()].deadline = dl;
    }
    block(TS_SLEEP, -2);
}

/// k_sem_take: block while the count is zero.
fn k_sem_take(which: usize) {
    loop {
        unsafe {
            let sem = if which == 0 {
                &mut *core::ptr::addr_of_mut!(SEM_A)
            } else {
                &mut *core::ptr::addr_of_mut!(SEM_B)
            };
            if sem.count > 0 {
                sem.count -= 1;
                return;
            }
            sem.waiters[sem.n_waiters] = CUR;
            sem.n_waiters += 1;
        }
        block(TS_SEM, which as i32);
    }
}

/// k_sem_give: bump the count and wake the oldest waiter.
fn k_sem_give(which: usize) {
    unsafe {
        let sem = if which == 0 {
            &mut *core::ptr::addr_of_mut!(SEM_A)
        } else {
            &mut *core::ptr::addr_of_mut!(SEM_B)
        };
        sem.count += 1;
        wake_waiter(&mut sem.waiters, &mut sem.n_waiters);
    }
}

/// k_msgq_init.
fn k_msgq_init(q: *mut Msgq, size: usize, depth: usize) {
    unsafe {
        (*q).size = size.min(64 / depth.max(1));
        (*q).depth = depth.min(64 / size.max(1));
        (*q).buf = [0; 64];
        (*q).head = 0;
        (*q).count = 0;
        (*q).n_get = 0;
        (*q).n_put = 0;
    }
}

/// k_msgq_put: block while full; wake a blocked getter.
fn k_msgq_put(q: *mut Msgq, data: &[u8]) {
    loop {
        unsafe {
            let mq = &mut *q;
            if mq.count < mq.depth {
                let idx = (mq.head + mq.count) % mq.depth;
                let slot = idx * mq.size;
                let n = data.len().min(mq.size);
                mq.buf[slot..slot + n].copy_from_slice(&data[..n]);
                mq.count += 1;
                wake_waiter(&mut mq.get_waiters, &mut mq.n_get);
                return;
            }
            mq.put_waiters[mq.n_put] = CUR;
            mq.n_put += 1;
        }
        block(TS_PUT, 0);
    }
}

/// k_msgq_get: block while empty; wake a blocked putter.
fn k_msgq_get(q: *mut Msgq, out: &mut [u8]) -> usize {
    loop {
        unsafe {
            let mq = &mut *q;
            if mq.count > 0 {
                let slot = mq.head * mq.size;
                let n = out.len().min(mq.size);
                out[..n].copy_from_slice(&mq.buf[slot..slot + n]);
                mq.head = (mq.head + 1) % mq.depth;
                mq.count -= 1;
                wake_waiter(&mut mq.put_waiters, &mut mq.n_put);
                return n;
            }
            mq.get_waiters[mq.n_get] = CUR;
            mq.n_get += 1;
        }
        block(TS_GET, 0);
    }
}

/// k_thread_create: seed a fresh thread context (entry wrapper, private
/// stack, x19 = thread id).
fn k_thread_create(entry: usize, prio: u8, tid: usize) {
    unsafe {
        let stack_top =
            core::ptr::addr_of!(THREAD_STACKS) as usize + tid * 8192 + 8192;
        THREADS[tid].state = TS_READY;
        THREADS[tid].prio = prio;
        THREADS[tid].stack_top = stack_top;
        THREADS[tid].wait = -1;
        THREADS[tid].deadline = 0;
        THREADS[tid].rounds = 0;
        THREADS[tid].jb = GuestJb {
            x19x28: [tid as u64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            fp: 0,
            lr: entry as u64,
            sp: stack_top as u64,
        };
    }
}

/// Thread wrapper: every thread starts here (jb was seeded with
/// `lr = tcb_entry`), takes its id from x19 and runs `demo_thread`.
fn tcb_entry() -> ! {
    let tid: u64;
    unsafe {
        core::arch::asm!("mov {}, x19", out(reg) tid, options(nomem, nostack));
    }
    let tid = tid as usize;
    puts("[rtos] thread ");
    put_u32(tid as u32);
    puts(" entered\n");
    demo_thread(tid);
    k_thread_exit();
}

/// k_thread_exit: mark the caller exited and dispatch away.
fn k_thread_exit() -> ! {
    unsafe {
        THREADS[cur_tid()].state = TS_EXIT;
    }
    dispatch();
    unreachable!()
}

// ── VirtIO ring (mirrors the kernel's virtio/mod.rs layout) ──────────────────

const VIRTQ_MAGIC: u32 = 0x5649_5254;
const QUEUE_SIZE: usize = 16;
const BUF_SIZE: usize = 256;
const OFF_CONFIG: usize = 0x0000;
const OFF_DESC: usize = 0x0040;
const OFF_AVAIL: usize = OFF_DESC + QUEUE_SIZE * 16; // 0x0140
const OFF_USED: usize = 0x1000;

const OP_PRINT: u8 = 0x01;
const OP_ECHO: u8 = 0x02;

const VMM_INFO_OFF: usize = 0x3000;
const VMM_INFO_MAGIC: u32 = 0x564D_4D49; // "IVMM"
const TENANT_ID_OFF: usize = 0x4;
const GUEST_STATE_OFF: usize = 0x10;
const GUEST_RUNNING: u32 = 0;
const GUEST_PARKED: u32 = 2;

unsafe fn vq_avail_idx(base: *mut u8) -> u16 {
    let avail = base.add(OFF_AVAIL) as *const u16;
    core::ptr::read_volatile(avail.add(1))
}

unsafe fn vq_avail_ring(base: *mut u8, slot: usize) -> u16 {
    let avail = base.add(OFF_AVAIL) as *const u16;
    core::ptr::read_volatile(avail.add(2 + slot))
}

unsafe fn vq_desc_addr(base: *mut u8, idx: usize) -> u64 {
    let desc = base.add(OFF_DESC + idx * 16) as *const u64;
    core::ptr::read_volatile(desc)
}

unsafe fn vq_put_used(base: *mut u8, last_used: &mut u16, desc_idx: u16, written: u32) {
    let used_base = base.add(OFF_USED) as *mut u16;
    let ring_elem = base.add(OFF_USED + 4 + (*last_used as usize % QUEUE_SIZE) * 8);
    core::ptr::write_volatile(ring_elem as *mut u32, desc_idx as u32);
    core::ptr::write_volatile(ring_elem.add(4) as *mut u32, written);
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    *last_used = last_used.wrapping_add(1);
    core::ptr::write_volatile(used_base.add(1), *last_used);
}

static mut LAST_AVAIL: u16 = 0;
static mut LAST_USED: u16 = 0;

/// Drain the kernel's avail ring: print every Print payload and Echo it
/// back.  Runs from the idle thread (the guest's lowest-priority work).
unsafe fn drain_virtq(base: *mut u8, tenant_id: u8) {
    let mut handled = 0;
    loop {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let avail_idx = vq_avail_idx(base);
        if avail_idx == LAST_AVAIL {
            break;
        }
        let slot = LAST_AVAIL as usize % QUEUE_SIZE;
        let desc_idx = vq_avail_ring(base, slot);
        LAST_AVAIL = LAST_AVAIL.wrapping_add(1);

        let buf_addr = vq_desc_addr(base, desc_idx as usize);
        let buf = buf_addr as *mut u8;

        let opcode = core::ptr::read_volatile(buf);
        if opcode == OP_PRINT {
            let payload_len = core::ptr::read_volatile(buf.add(3)) as usize;
            let payload =
                core::slice::from_raw_parts(buf.add(4), payload_len.min(BUF_SIZE - 4));

            puts("[T");
            put_u32(tenant_id as u32);
            puts(" vq] ");
            for &b in payload {
                putc(b);
            }
            puts("\n");

            // Echo reply: opcode, reserved×2, length=4, u32 LE count.
            core::ptr::write_volatile(buf, OP_ECHO);
            core::ptr::write_volatile(buf.add(1), 0u8);
            core::ptr::write_volatile(buf.add(2), 0u8);
            core::ptr::write_volatile(buf.add(3), 4u8);
            buf.add(4).copy_from((payload_len as u32).to_le_bytes().as_ptr(), 4);

            vq_put_used(base, &mut LAST_USED, desc_idx, 8u32);
            handled += 1;
        } else {
            vq_put_used(base, &mut LAST_USED, desc_idx, 0u32);
        }
        if handled >= QUEUE_SIZE {
            break; // never spin forever on a dead ring
        }
    }
}

// ── Demo threads ──────────────────────────────────────────────────────────────

const DEMO_MSGS: usize = 12;
const TIMER_ROUNDS: u32 = 12;

/// Producer: rendezvous via k_sem (waits for the consumer's token),
/// publishes "m<i>" into the demo queue, then sleeps.  Exercises
/// k_sem_take + k_msgq_put + k_sleep.
fn producer(_tid: usize) {
    for i in 0..DEMO_MSGS {
        k_sem_take(1); // SEM_B: consumer gave us its token → next message
        let msg = [b'm', b'0' + (i % 10) as u8, 0, 0, 0, 0, 0, 0];
        k_msgq_put(core::ptr::addr_of_mut!(MSGQ_DEMO), &msg);
        puts("[rtos] prod: put m");
        put_u32(i as u32);
        puts(" -> k_msgq\n");
        k_sleep(2);
        k_yield();
    }
    puts("[rtos] prod: done\n");
}

/// Consumer: pulls messages off the queue (blocks when empty), gives the
/// producer its token (k_sem_give), then sleeps.
fn consumer(_tid: usize) {
    let mut got = 0usize;
    while got < DEMO_MSGS {
        let mut m = [0u8; 8];
        let n = k_msgq_get(core::ptr::addr_of_mut!(MSGQ_DEMO), &mut m);
        got += 1;
        k_sem_give(1); // SEM_B token back to the producer
        puts("[rtos] cons: got ");
        put_u32(got as u32);
        puts(" n=");
        put_u32(n as u32);
        puts(" '");
        for &b in &m[..n.min(4)] {
            putc(b);
        }
        puts("'\n");
        k_sleep(1);
        k_yield();
    }
    puts("[rtos] cons: done\n");
}

/// Timer thread: k_timer-style counting rounds — sleeps 5 ms per round.
fn timer_thread(_tid: usize) {
    for round in 0..TIMER_ROUNDS {
        k_sleep(5);
        puts("[rtos] timer: k_timer round ");
        put_u32(round + 1);
        puts("\n");
    }
    puts("[rtos] timer: done\n");
}

fn demo_thread(tid: usize) {
    match tid {
        0 => producer(tid),
        1 => consumer(tid),
        _ => timer_thread(tid),
    }
}

// ── Idle thread: guest tickless idle + kernel tenant yielding ────────────────

fn all_threads_exited() -> bool {
    unsafe {
        for t in &THREADS {
            if t.state != TS_EXIT {
                return false;
            }
        }
        true
    }
}

/// Earliest sleeping thread's deadline (None if nobody sleeps).
fn earliest_deadline() -> Option<u64> {
    unsafe {
        let mut dl: Option<u64> = None;
        for t in &THREADS {
            if t.state == TS_SLEEP {
                dl = Some(dl.map_or(t.deadline, |d: u64| d.min(t.deadline)));
            }
        }
        dl
    }
}

/// Re-ready every sleeper whose deadline has passed.
fn wake_expired_sleepers() {
    unsafe {
        let now = cntvct();
        for t in THREADS.iter_mut() {
            if t.state == TS_SLEEP && t.deadline <= now {
                t.state = TS_READY;
            }
        }
    }
}

unsafe fn guest_idle(base: *mut u8, tenant_id: u8, yield_fn: YieldFn, guest_ctx: u64) -> ! {
    // Publish RUNNING once the RTOS is alive, then enter the tickless
    // idle loop.
    core::ptr::write_volatile(
        (base.add(VMM_INFO_OFF + GUEST_STATE_OFF)) as *mut u32,
        GUEST_RUNNING,
    );

    loop {
        // Checkpoint: `dispatch()` (from a blocked thread) resumes us here
        // whenever no thread is ready; a kernel preemption/resume lands
        // here too (the VMM restores the interrupted PC, i.e. just after
        // this save).
        let mut jb = IDLE_JB;
        let _ = gs_save(&mut jb);
        IDLE_JB = jb;

        // A thread may have been woken by another thread's give/put since
        // our last pass — give it the CPU now.
        if pick_ready() >= 0 {
            dispatch(); // never returns while threads run
        }

        // Guest idle: kernel channel first, sleeping threads next.
        drain_virtq(base, tenant_id);

        if all_threads_exited() {
            puts("[rtos] all threads done — parking\n");
            core::ptr::write_volatile(
                (base.add(VMM_INFO_OFF + GUEST_STATE_OFF)) as *mut u32,
                GUEST_PARKED,
            );
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            yield_fn(guest_ctx as usize);
            loop {
                core::hint::spin_loop(); // parked — kernel dropped us
            }
        }

        if let Some(dl) = earliest_deadline() {
            let now = cntvct();
            if now < dl {
                // Tickless idle: hold the vCPU until the earliest wake.
                // The VMM's tick preempts us meanwhile (co-tenancy) and we
                // resume here on the next slice — the spin picks up where
                // it left off.
                while cntvct() < dl {
                    core::hint::spin_loop();
                }
            }
            wake_expired_sleepers();
            continue; // re-checkpoint: dispatcher runs the woken thread
        }

        // Nothing to do and nobody asleep — hand this tenant's time back
        // to the kernel's tenant scheduler.
        yield_fn(guest_ctx as usize);
    }
}

// ── Boot ──────────────────────────────────────────────────────────────────────

fn read_boot_arg(reg: &str) -> u64 {
    let v: u64;
    unsafe {
        match reg {
            "x4" => core::arch::asm!("mov {}, x4", out(reg) v, options(nomem, nostack)),
            "x5" => core::arch::asm!("mov {}, x5", out(reg) v, options(nomem, nostack)),
            "x6" => core::arch::asm!("mov {}, x6", out(reg) v, options(nomem, nostack)),
            "x7" => core::arch::asm!("mov {}, x7", out(reg) v, options(nomem, nostack)),
            _ => unreachable!(),
        }
    }
    v
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let shmem_phys = read_boot_arg("x4");
    let yield_addr = read_boot_arg("x5");
    let guest_ctx = read_boot_arg("x6");
    set_uart_base(read_boot_arg("x7"));

    let base = shmem_phys as *mut u8;

    // Tenant id published by the kernel (info block +4).
    let tenant_id = unsafe {
        core::ptr::read_volatile((base.add(TENANT_ID_OFF)) as *const u32)
    };

    // The kernel's info block must be present before we publish anything.
    loop {
        let magic = unsafe { core::ptr::read_volatile(base.add(VMM_INFO_OFF) as *const u32) };
        if magic == VMM_INFO_MAGIC {
            break;
        }
        core::hint::spin_loop();
    }

    puts("\n[T");
    put_u32(tenant_id);
    puts("] RTOS guest booted (shmem=");
    put_u32_hex(shmem_phys as u32);
    puts(")\n");

    // Real RTOS init: zero the tables (defensive; RAM is pre-zeroed).
    unsafe {
        THREADS = [tcb_zeroed(); MAX_THREADS];
        SEM_A = sem_zeroed();
        SEM_B = sem_zeroed();
        MSGQ_DEMO = msgq_zeroed();
        CUR = -1;
        SEM_B.count = 1; // producer's initial token
    }
    k_msgq_init(core::ptr::addr_of_mut!(MSGQ_DEMO), 8, 8);

    // Spawn the demo threads: producer (0), consumer (1), timer (2).
    unsafe {
        k_thread_create(tcb_entry as usize, 0, 0);
        k_thread_create(tcb_entry as usize, 1, 1);
        k_thread_create(tcb_entry as usize, 2, 2);
    }
    puts("[rtos] scheduler: 3 threads ready; entering idle loop\n");

    // The boot context becomes the guest's idle thread.
    let yield_fn: YieldFn = unsafe { core::mem::transmute(yield_addr) };
    unsafe { guest_idle(base, tenant_id as u8, yield_fn, guest_ctx) }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // First: dump the raw register file + ESR, BEFORE touching any local
    // memory — the register state AT the original panic may itself be
    // corrupted (a resumed thread with a bad jb), and the formatter
    // machinery must not be trusted.
    raw_reg_dump();
    // Site + message: `as_str()` covers const-string panics; formatted ones
    // (index-out-of-bounds etc) at least yield their location.  No fmt.
    puts("\n[rtos] PANIC at ");
    match info.message().as_str() {
        Some(s) => puts(s),
        None => puts("(formatted msg)"),
    }
    // Raw PanicInfo words: [0]=&Arguments, [8]=&Location.  The Arguments
    // struct's pieces/data pointers are absolute (base-0) addresses from the
    // link, so never dereference them; print the raw words instead.
    let p = info as *const PanicInfo as *const u64;
    for i in 0..4 {
        puts(" I");
        put_u32(i);
        puts("x");
        put_u64_hex(unsafe { *p.add(i as usize) });
    }
    if let Some(loc) = info.location() {
        puts(" Lfile=");
        put_u64_hex(loc.file().as_ptr() as usize as u64);
        puts(" Lline=");
        put_u32(loc.line() as u32);
        puts(" Lcol=");
        put_u32(loc.column() as u32);
    }
    puts(" CUR=");
    unsafe { put_u32(CUR as u32) };
    for tid in 0..MAX_THREADS {
        let t = unsafe { &THREADS[tid] };
        puts(" T");
        put_u32(tid as u32);
        puts("s");
        put_u32(t.state as u32);
        puts("p");
        put_u32(t.prio as u32);
        puts("w");
        put_u32(t.wait as u32);
        puts("d");
        put_u32((t.deadline & 0xFFFFFFFF) as u32);
    }
    let mq = unsafe { &MSGQ_DEMO };
    puts(" MQ{head=");
    put_u32(mq.head as u32);
    puts(" cnt=");
    put_u32(mq.count as u32);
    puts(" ng=");
    put_u32(mq.n_get as u32);
    puts(" np=");
    put_u32(mq.n_put as u32);
    puts("}");
    puts("\n");
    // Best-effort: publish PARKED so the VMM drops this tenant instead of
    // spinning on a dead guest.
    let shmem_phys = read_boot_arg("x4");
    unsafe {
        core::ptr::write_volatile(
            (shmem_phys as usize + VMM_INFO_OFF + GUEST_STATE_OFF) as *mut u32,
            GUEST_PARKED,
        );
    }
    loop {
        core::hint::spin_loop();
    }
}