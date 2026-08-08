//! Kernel-side implementation of the Phase-6 SVC syscall interface.
//!
//! Servers run at EL0 in their own address spaces (per-task TTBR0) and call
//! the kernel with `svc #0`: the syscall number in x0, arguments in x1-x3,
//! result in x0 (vectors.s dispatches EC 0x15 to `tanix_syscall`).
//!
//! IPC model (Minix `send` / `receive`):
//!   • `send(dst, msg)` blocks the sender until the receiver *accepts* the
//!     message (rendezvous).  The kernel copies the message, so the sender's
//!     buffer only needs to be valid while the sender is blocked.
//!   • `receive(filter, out)` blocks until a message from a matching sender
//!     arrives; the kernel copies it into `out` and stamps `src`.
//!   • A sender that cannot rendezvous immediately parks itself in the
//!     receiver's `pending_senders` queue and blocks until the receiver
//!     picks the message up.
//!
//! Address-space notes (Phase 6):
//!   • The kernel runs with the *current* task's TTBR0 active.  The task's
//!     own pages are mapped EL1-accessible (AP 0b01), so pointers the task
//!     passes in are directly dereferenceable — except when the target of
//!     the copy is another task's memory (receive buffers of blocked
//!     receivers).  Those are written under the receiver's own table via
//!     `with_ttbr0`.
//!   • `PendingSend` stores the message *by value*, copied while the
//!     sender's table is still active, so no cross-address-space pointers
//!     are kept.
//!   • `alloc_frames` / `free_frames` map / demote the frames in the
//!     *calling* task's table (identity VA == phys), so the returned
//!     physical base stays directly dereferenceable by the server.

use crate::mem::{frame, page_table, PAGE_SIZE};
use crate::sched::task::{context_switch, current_ttbr0, kill_task};
use crate::sched::{Message, PendingSend, StagedSend, TaskId, TaskState, M_ANY};

// ── Syscall numbers (must match `servers/libtanix-sys/src/sys.rs`) ────────────

pub const SYS_SEND: u64 = 0;
pub const SYS_RECEIVE: u64 = 1;
pub const SYS_SPAWN: u64 = 2;
pub const SYS_WHO: u64 = 3;
pub const SYS_EXIT_TASK: u64 = 4;
pub const SYS_EXIT: u64 = 5;
pub const SYS_ALLOC_FRAMES: u64 = 6;
pub const SYS_FREE_FRAMES: u64 = 7;
pub const SYS_LOG: u64 = 8;
pub const SYS_WAIT_IRQ: u64 = 9; // Phase 7: block until a device IRQ fires
pub const SYS_YIELD: u64 = 10; // Phase 7: cooperative yield (RR rotation)
pub const SYS_SHARE_FRAMES: u64 = 11; // Phase 8: map frames into another task's table
pub const SYS_UNSHARE_FRAMES: u64 = 12; // Phase 8: demote frames in another task's table
pub const SYS_SLEEP: u64 = 13; // Phase 8: block until the tick counter passes a deadline

// ── Active TTBR0 helpers ──────────────────────────────────────────────────────

/// Read the currently active TTBR0_EL1.
unsafe fn active_ttbr0() -> u64 {
    let t: u64;
    core::arch::asm!("mrs {0}, ttbr0_el1", out(reg) t, options(nomem, nostack));
    t
}

/// Run `f` with the task table `ttbr0` active (the kernel's own table when
/// 0), restoring the previous table afterwards.  Each switch flushes the
/// TLB (we use no ASIDs).
///
/// Safe to use for *live* tasks only; `ttbr0` must not be a dead task's
/// (freed) tables.
///
/// # Safety
/// Must be called from EL1.
unsafe fn with_ttbr0<T>(ttbr0: u64, f: impl FnOnce() -> T) -> T {
    let cur = active_ttbr0();
    if ttbr0 != cur {
        core::arch::asm!(
            "msr ttbr0_el1, {0}",
            "isb",
            in(reg) ttbr0,
            options(nomem, nostack)
        );
        page_table::flush_tlb();
    }
    let r = f();
    if ttbr0 != cur {
        core::arch::asm!(
            "msr ttbr0_el1, {0}",
            "isb",
            in(reg) cur,
            options(nomem, nostack)
        );
        page_table::flush_tlb();
    }
    r
}

// ── IPC ───────────────────────────────────────────────────────────────────────

/// Deliver `msg` (a pointer valid under the *current* table — the
/// sender's) into `dst`'s receive buffer, stamping `src`.
///
/// `dst.recv_buf` lives in the receiver's own address space, so the copy
/// is performed with the receiver's table temporarily active.
///
/// # Safety
/// `dst` must be a live, blocked-in-receive task; `msg` must point to a
/// valid message in the caller's address space.
unsafe fn deliver(dst: &mut crate::sched::task::Task, src: u32, msg: *const Message) {
    let out = dst.recv_buf;
    let saved = *msg; // read under the current (sender's) table
    with_ttbr0(dst.ttbr0, || {
        let mut m = saved;
        m.src = src;
        core::ptr::write_volatile(out, m);
    });
    dst.recv_blocked = false;
    dst.state = TaskState::Ready;
}

/// `send(dst, msg)` syscall.
unsafe fn sys_send(dst: u32, msg: *const Message) -> i32 {
    let sched = crate::sched::task::scheduler();
    let me = sched.current_id().0;
    let my_idx = sched.current_idx();

    if dst == me {
        return -2; // self-send is an error
    }
    let dst_idx = match sched.task_idx(TaskId(dst)) {
        Some(i) => i,
        None => return -3, // no such task
    };

    let (dst_blocked, dst_filter) = {
        let t = sched.task_at(dst_idx).expect("task index out of range");
        (t.recv_blocked, t.recv_filter)
    };

    if dst_blocked && (dst_filter == M_ANY || dst_filter == me as i32) {
        // Rendezvous: receiver is waiting for us — copy directly.
        let dst_task = sched.task_at_mut(dst_idx).unwrap();
        deliver(dst_task, me, msg);
        sched.set_woken(dst_idx);
        log::trace!("ipc: {} → {} rendezvous (direct)", me, dst);
        return 0;
    }

    // Receiver not waiting — park ourselves as a pending sender.  Copy the
    // message now, while our table is still active (the receiver runs on a
    // different TTBR0 and must not dereference our pointers later).
    //
    // If the receiver's send queue is full we never drop the message: we
    // block in a queue-wait state and the receiver's `receive` delivers the
    // staged message directly the moment its filter matches (see
    // `sys_receive`).  Dropping a reply because the queue was full would
    // deadlock request/response pairs (the wm once lost the display's
    // MODE_REPLY this way).
    let queued = {
        let dst_task = sched.task_at_mut(dst_idx).unwrap();
        match dst_task.pending_senders.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(PendingSend { src: me, msg: *msg });
                true
            }
            None => false, // receiver's send queue full
        }
    };
    if !queued {
        // Queue full — stage the message on ourselves and block.  The only
        // way out is the receiver's `receive` delivering it directly, so a
        // resumed send knows its message was consumed.
        log::trace!("ipc: {} → {} queue full, staging send", me, dst);
        {
            let t = sched.task_at_mut(my_idx).unwrap();
            t.send_full_wait = Some(StagedSend { dst, src: me, msg: *msg });
            t.state = TaskState::Blocked;
        }
        let next = sched.pick_next();
        sched.set_state(next, TaskState::Running);
        sched.set_current(next);
        context_switch(sched.ctx_ptr(my_idx), sched.ctx_ptr(next));
        return 0; // delivered by the receiver's receive
    }

    log::trace!("ipc: {} → {} queued, sender blocks", me, dst);
    let next = {
        sched.set_state(my_idx, TaskState::Blocked);
        sched.pick_next()
    };
    sched.set_state(next, TaskState::Running);
    sched.set_current(next);
    context_switch(sched.ctx_ptr(my_idx), sched.ctx_ptr(next));
    0
}

/// `receive(filter, out)` syscall.  Returns the sender's id (or -errno).
unsafe fn sys_receive(filter: i32, out: *mut Message) -> i32 {
    let sched = crate::sched::task::scheduler();
    let my_idx = sched.current_idx();
    let my_id = sched.current_id().0;

    // Any pending sender already waiting for us?  The parked message is a
    // value copy, valid under *our* table — no switch needed.
    let pending = {
        let t = sched.task_at_mut(my_idx).unwrap();
        let mut found = None;
        for slot in t.pending_senders.iter_mut() {
            if let Some(p) = *slot {
                if filter == M_ANY || p.src == filter as u32 {
                    *slot = None;
                    found = Some(p);
                    break;
                }
            }
        }
        found
    };
    if let Some(p) = pending {
        let mut m = p.msg;
        m.src = p.src;
        core::ptr::write_volatile(out, m);
        if let Some(src_idx) = sched.task_idx(TaskId(p.src)) {
            sched.task_at_mut(src_idx).unwrap().state = TaskState::Ready;
            sched.set_woken(src_idx);
            log::trace!("ipc: {} wakes pending sender {}", my_idx, p.src);
        }
        return p.src as i32;
    }

    // Nothing parked matches our filter.  A sender may be staged on us
    // because its send hit our full queue before we ever got here — deliver
    // the staged message directly instead of blocking (the sender keeps
    // `send_full_wait` set until this point).
    for idx in 1..crate::sched::task::MAX_TASKS {
        let Some(t) = sched.task_at_mut(idx) else {
            continue;
        };
        let hit = t.send_full_wait.is_some_and(|s| {
            s.dst == my_id && (filter == M_ANY || s.src == filter as u32)
        });
        if hit {
            let s = t.send_full_wait.take().unwrap();
            let mut m = s.msg;
            m.src = s.src;
            core::ptr::write_volatile(out, m);
            t.state = TaskState::Ready;
            sched.set_woken(idx);
            log::trace!("ipc: {} delivers staged send from {}", my_idx, s.src);
            return s.src as i32;
        }
    }

    // Nothing available — block until a sender rendezvouses with us.  Our
    // `out` buffer is stored as a pointer into *our* address space; the
    // kernel switches to our table when it must write it.
    {
        let t = sched.task_at_mut(my_idx).unwrap();
        t.recv_blocked = true;
        t.recv_filter = filter;
        t.recv_buf = out;
        t.state = TaskState::Blocked;
    }
    let next = sched.pick_next();
    sched.set_state(next, TaskState::Running);
    sched.set_current(next);
    context_switch(sched.ctx_ptr(my_idx), sched.ctx_ptr(next));

    // Resumed: the sender already copied the message into our buffer.
    (*out).src as i32
}

// ── Process-management syscalls ───────────────────────────────────────────────

/// Read a NUL-terminated string from the caller's address space (the
/// caller's table is active, so plain reads are fine) into `buf`, returning
/// it as a `&str` (or `None` if it is too long / not valid UTF-8).
unsafe fn read_cstr(mut p: *const u8, buf: &mut [u8]) -> Option<&str> {
    let mut n = 0usize;
    while n < buf.len() {
        let b = *p;
        if b == 0 {
            return core::str::from_utf8(&buf[..n]).ok();
        }
        buf[n] = b;
        n += 1;
        p = p.add(1);
    }
    None
}

/// `spawn(name) -> new task id | -errno`.  Name is a NUL-terminated string
/// in the caller's memory (the caller's table is active — plain deref).
unsafe fn sys_spawn(name: *const u8) -> i32 {
    let mut buf = [0u8; 16];
    let Some(s) = read_cstr(name, &mut buf) else {
        return -5; // too long / not UTF-8
    };
    match crate::server::spawn_by_name(s) {
        Ok(id) => id.0 as i32,
        Err(e) => e,
    }
}

/// `who(name) -> task id | -1`.
unsafe fn sys_who(name: *const u8) -> i32 {
    let mut buf = [0u8; 16];
    let Some(s) = read_cstr(name, &mut buf) else {
        return -1;
    };
    let sched = crate::sched::task::scheduler();
    for t in sched.task_slots().iter().flatten() {
        if t.name_str() == s && t.state != TaskState::Zombie {
            return t.id.0 as i32;
        }
    }
    -1
}

/// `exit_task(pid)` — kill another task.
unsafe fn sys_exit_task(pid: u32) -> i32 {
    if kill_task(TaskId(pid)) {
        0
    } else {
        -3 // no such task
    }
}

/// `exit()` — terminate the calling task.
unsafe fn sys_exit() -> ! {
    crate::sched::task::exit_current()
}

// ── Memory syscalls ───────────────────────────────────────────────────────────

/// `alloc_frames(n) -> physical base | 0`.
///
/// The frames are mapped into the *calling task's* address space at their
/// physical address (identity), so the returned base is directly usable —
/// same ABI as Phase 4, where servers simply dereferenced the physical
/// addresses the kernel handed out.
unsafe fn sys_alloc_frames(pages: u32) -> u64 {
    let base = match frame::alloc_frames(pages as usize) {
        Some(b) => b,
        None => return 0,
    };
    let ttbr0 = current_ttbr0();
    if ttbr0 != 0 {
        page_table::map_user_pages(ttbr0 as usize, base, pages as usize * PAGE_SIZE, page_table::FLAGS_USER_RWX);
        page_table::flush_tlb();
    }
    log::trace!("alloc_frames: {} pages → {:#x} (ttbr0={:#x})", pages, base, ttbr0);
    base as u64
}

/// `free_frames(base, n)`.
///
/// Demotes the pages back to EL1-only in the calling task's table before
/// releasing the frames (the frames' contents stay valid for the kernel).
unsafe fn sys_free_frames(base: u64, pages: u32) -> i32 {
    let ttbr0 = current_ttbr0();
    for i in 0..pages as usize {
        let pa = base as usize + i * PAGE_SIZE;
        if ttbr0 != 0 {
            page_table::map_user_pages(ttbr0 as usize, pa, PAGE_SIZE, page_table::FLAGS_KERNEL_RWX);
        }
        frame::free_frame(pa);
    }
    if ttbr0 != 0 {
        page_table::flush_tlb();
    }
    0
}

/// `share_frames(base, pages, task)` — Phase 8.
///
/// Maps the physical run `base .. base + pages*PAGE_SIZE` into task
/// `task`'s address space as EL0-visible (identity VA == phys), so two
/// servers can share a buffer (the window compositor blits canvases that
/// the owning app allocated).  The frames are NOT re-allocated: the caller
/// must own them (`alloc_frames`) and the target must not already map them.
unsafe fn sys_share_frames(base: u64, pages: u32, task: u32) -> i32 {
    if base == 0 || pages == 0 || pages > 4096 {
        return -11;
    }
    let sched = crate::sched::task::scheduler();
    let ttbr0 = match sched.task_by_id(TaskId(task)) {
        Some(dst) => dst.ttbr0,
        None => return -3, // no such task
    };
    if ttbr0 == 0 {
        return 0; // kernel-side task — nothing to map
    }
    with_ttbr0(ttbr0, || {
        page_table::map_user_pages(
            ttbr0 as usize,
            base as usize,
            pages as usize * PAGE_SIZE,
            page_table::FLAGS_USER_RWX,
        );
    });
    page_table::flush_tlb();
    log::trace!(
        "share_frames: {:#x}+{} pages → task {}",
        base, pages, task
    );
    0
}

/// `unshare_frames(base, pages, task)` — Phase 8 mirror: demote the run
/// back to EL1-only in `task`'s table (the owning task keeps its own
/// mapping; free the frames only after unsharing).
unsafe fn sys_unshare_frames(base: u64, pages: u32, task: u32) -> i32 {
    if base == 0 || pages == 0 || pages > 4096 {
        return -11;
    }
    let sched = crate::sched::task::scheduler();
    let ttbr0 = match sched.task_by_id(TaskId(task)) {
        Some(dst) => dst.ttbr0,
        None => return -3,
    };
    if ttbr0 == 0 {
        return 0;
    }
    with_ttbr0(ttbr0, || {
        for i in 0..pages as usize {
            page_table::map_user_pages(
                ttbr0 as usize,
                base as usize + i * PAGE_SIZE,
                PAGE_SIZE,
                page_table::FLAGS_KERNEL_RWX,
            );
        }
    });
    page_table::flush_tlb();
    0
}

// ── IRQ syscalls (Phase 7) ────────────────────────────────────────────────────

/// `wait_irq(irq) -> 0`.  Blocks the calling task until the device
/// interrupt `irq` has been delivered; the interrupt is enabled in the GIC
/// lazily on the first wait.
///
/// Implementation notes:
///   • The caller is parked in a *kernel* wait loop.  IRQs are masked
///     whenever the CPU is in the kernel, so the loop runs with IRQs
///     unmasked (`daifclr #2`) and sleeps in `wfi` — the device IRQ (or the
///     tick) wakes the core, `irq_handler` records the pending bit, and the
///     loop re-tests it.  The handler never switches away from this loop
///     (its `from_el0` is false), so the waiter's kernel stack stays
///     consistent.
///   • An IRQ that arrived *before* the wait is recorded by the handler and
///     returns immediately (test-and-clear first).
unsafe fn sys_wait_irq(irq: u32) -> i32 {
    let sched = crate::sched::task::scheduler();
    let my_idx = sched.current_idx();

    // PPIs 16..31, SPIs 32..1019 — anything the GIC can deliver.
    if !(16..=1019).contains(&irq) {
        return -10; // invalid IRQ
    }
    crate::arch::aarch64::gic::enable_irq(irq);

    if crate::irq::take_pending(irq) {
        return 0; // already serviced
    }

    sched.task_at_mut(my_idx).unwrap().irq_wait = Some(irq);

    // Wait loop: interrupts unmasked, sleeping in wfi.
    //
    // The pending check is the FIRST instruction after `daifclr`: if the
    // IRQ was already asserted (level-triggered device, completed request
    // before we armed the GIC), the delivery fires immediately after
    // unmasking and the handler records the bit *before* eret — so the
    // re-executed check observes it.  With the check placed after the
    // `wfi` instead, eret would resume at the `wfi` (the IRQ can land at
    // any instruction, even the branch before it) and sleep forever: the
    // level is high but `irq_handler` disabled the SPI, so no wake ever
    // comes again.
    core::arch::asm!("msr daifclr, #2", options(nomem, nostack)); // clear I
    loop {
        if crate::irq::pending(irq) {
            break;
        }
        core::arch::asm!("wfi", options(nomem, nostack));
    }
    core::arch::asm!("msr daifset, #2", options(nomem, nostack)); // set I
    let t = sched.task_at_mut(my_idx).unwrap();
    t.irq_wait = None;
    crate::irq::clear_pending(irq);
    0
}

// ── Sleep syscall (Phase 8) ───────────────────────────────────────────────────

/// `sleep(ms) -> 0`.  Blocks the calling task until `ms` scheduler ticks
/// (1 ms each, Phase 7) have elapsed, then returns.
///
/// The deadline is stored in the task's `sleep_deadline`; the timer-tick
/// IRQ handler wakes expired sleepers (`task::tick_preempt`).  Waking is
/// purely a state change — the CPU is handed over here and picked up again
/// by the regular scheduling points (next tick, next syscall tail), so a
/// sleeping task never occupies the core.
unsafe fn sys_sleep(ms: u32) -> i32 {
    let ms = ms.clamp(1, 60_000);
    let sched = crate::sched::task::scheduler();
    let my_idx = sched.current_idx();
    let deadline = crate::arch::aarch64::timer::ticks().saturating_add(ms as u64);

    sched.task_at_mut(my_idx).unwrap().sleep_deadline = deadline;
    sched.set_state(my_idx, TaskState::Blocked);
    let next = sched.pick_next();
    sched.set_state(next, TaskState::Running);
    sched.set_current(next);
    context_switch(sched.ctx_ptr(my_idx), sched.ctx_ptr(next));

    // Resumed once the deadline passed (tick_preempt woke us).
    sched.task_at_mut(my_idx).unwrap().sleep_deadline = 0;
    0
}

// ── Logging syscall ───────────────────────────────────────────────────────────

/// `log(level, msg)` — kernel log line prefixed with the sender's name.
unsafe fn sys_log(level: u32, msg: *const u8) {
    let name = crate::sched::task::current_name();
    let len = {
        let mut n = 0usize;
        while *msg.add(n) != 0 {
            n += 1;
            if n > 255 {
                break;
            }
        }
        n
    };
    let text = core::str::from_utf8(core::slice::from_raw_parts(msg, len)).unwrap_or("?");
    match level {
        0 => log::info!("[{}] {}", name, text),
        1 => log::warn!("[{}] {}", name, text),
        _ => log::error!("[{}] {}", name, text),
    }
}

// ── SVC dispatcher ────────────────────────────────────────────────────────────

/// Phase-6 SVC dispatch entry, called from `vectors.s` (EC 0x15) with the
/// syscall number in x0 and arguments in x1-x3.  Returns the result in x0.
///
/// Must not clobber x30 (the stub preserves it for the caller) and must
/// return the result in x0.  x4+ are freely clobberable — the server-side
/// wrappers list them as clobbered.
#[no_mangle]
pub unsafe extern "C" fn tanix_syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let _ = a2; // reserved (Phase 6 ABI: x3 defined, unused so far)
    let result = match nr {
        SYS_SEND => sys_send(a0 as u32, a1 as *const Message) as u64,
        SYS_RECEIVE => sys_receive(a0 as i32, a1 as *mut Message) as u64,
        SYS_SPAWN => sys_spawn(a0 as *const u8) as u64,
        SYS_WHO => sys_who(a0 as *const u8) as u64,
        SYS_EXIT_TASK => sys_exit_task(a0 as u32) as u64,
        SYS_EXIT => {
            sys_exit();
        }
        SYS_ALLOC_FRAMES => sys_alloc_frames(a0 as u32),
        SYS_FREE_FRAMES => sys_free_frames(a0, a1 as u32) as u64,
        SYS_SHARE_FRAMES => sys_share_frames(a0, a1 as u32, a2 as u32) as u64,
        SYS_UNSHARE_FRAMES => sys_unshare_frames(a0, a1 as u32, a2 as u32) as u64,
        SYS_SLEEP => sys_sleep(a0 as u32) as u64,
        SYS_LOG => {
            sys_log(a0 as u32, a1 as *const u8);
            0
        }
        SYS_WAIT_IRQ => sys_wait_irq(a0 as u32) as u64,
        SYS_YIELD => {
            crate::sched::task::yield_cpu();
            0
        }
        other => {
            log::error!(
                "syscall: task '{}' invoked unknown syscall {}",
                crate::sched::task::current_name(),
                other
            );
            0
        }
    };
    // Phase 7: every syscall return re-evaluates the run queue.  A task
    // woken by this syscall (higher priority, or next in the RR rotation)
    // runs before we return to EL0.  When we are resumed, the saved
    // syscall frame continues here and hands `result` to the caller.
    crate::sched::task::reschedule();
    result
}
