//! Kernel-side implementation of the Phase-4 syscall table.
//!
//! Servers are separate binaries; they never link against the kernel.
//! Instead each server receives a `SyscallTable` pointer at boot and invokes
//! these functions through it.  The table is deliberately value-based (no
//! function addresses are resolvable from inside a server) — the same ABI
//! would be the SVC dispatch index once servers move to EL0.
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

use crate::mem::frame;
use crate::sched::task::{context_switch, kill_task, scheduler};
use crate::sched::{Message, PendingSend, TaskId, TaskState, M_ANY};

/// Deliver `msg` from `src` into `dst`'s receive buffer, stamping `src`.
///
/// # Safety
/// `dst.recv_buf` must be valid while `dst` is blocked in `receive`.
unsafe fn deliver(dst: &mut crate::sched::task::Task, src: u32, msg: *const Message) {
    let out = dst.recv_buf;
    // Copy first, then stamp: the sender's message carries its own (zeroed)
    // `src` field, which must not overwrite the stamp.
    core::ptr::copy_nonoverlapping(msg, out, 1);
    (*out).src = src;
    dst.recv_blocked = false;
    dst.state = TaskState::Ready;
}

/// `send(dst, msg)` syscall.
unsafe extern "C" fn sys_send(dst: u32, msg: *const Message) -> i32 {
    let sched = scheduler();
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
        log::trace!("ipc: {} → {} rendezvous (direct)", me, dst);
        return 0;
    }

    // Receiver not waiting — park ourselves as a pending sender.
    let queued = {
        let dst_task = sched.task_at_mut(dst_idx).unwrap();
        match dst_task.pending_senders.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(PendingSend { src: me, buf: msg });
                true
            }
            None => false, // receiver's send queue full
        }
    };
    if !queued {
        return -4;
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
unsafe extern "C" fn sys_receive(filter: i32, out: *mut Message) -> i32 {
    let sched = scheduler();
    let my_idx = sched.current_idx();

    // Any pending sender already waiting for us?
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
        // Copy from the (still blocked) sender's buffer, stamp, wake sender.
        core::ptr::copy_nonoverlapping(p.buf, out, 1);
        (*out).src = p.src;
        if let Some(src_idx) = sched.task_idx(TaskId(p.src)) {
            sched.task_at_mut(src_idx).unwrap().state = TaskState::Ready;
            log::trace!("ipc: {} wakes pending sender {}", my_idx, p.src);
        }
        return p.src as i32;
    }

    // Nothing available — block until a sender rendezvouses with us.
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

/// `spawn(name) -> new task id | -errno`.  Name is a NUL-terminated string
/// in the caller's memory (same address space, so a plain deref is fine).
unsafe extern "C" fn sys_spawn(name: *const u8) -> i32 {
    let len = {
        let mut n = 0usize;
        while *name.add(n) != 0 {
            n += 1;
            if n > 15 {
                return -5; // name too long
            }
        }
        n
    };
    let buf = core::slice::from_raw_parts(name, len);
    let s = match core::str::from_utf8(buf) {
        Ok(s) => s,
        Err(_) => return -6, // not UTF-8
    };
    match crate::server::spawn_by_name(s) {
        Ok(id) => id.0 as i32,
        Err(e) => e,
    }
}

/// `who(name) -> task id | -1`.
unsafe extern "C" fn sys_who(name: *const u8) -> i32 {
    let len = {
        let mut n = 0usize;
        while *name.add(n) != 0 {
            n += 1;
            if n > 15 {
                return -1;
            }
        }
        n
    };
    let buf = core::slice::from_raw_parts(name, len);
    let s = match core::str::from_utf8(buf) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let sched = scheduler();
    for t in sched.task_slots().iter().flatten() {
        if t.name_str() == s && t.state != TaskState::Zombie {
            return t.id.0 as i32;
        }
    }
    -1
}

/// `exit_task(pid)` — kill another task.
unsafe extern "C" fn sys_exit_task(pid: u32) -> i32 {
    if kill_task(TaskId(pid)) {
        0
    } else {
        -3 // no such task
    }
}

/// `exit()` — terminate the calling task.
unsafe extern "C" fn sys_exit() -> ! {
    crate::sched::task::exit_current()
}

// ── Memory syscalls (behind the mem server) ───────────────────────────────────

/// `alloc_frames(n) -> physical base | 0`.
unsafe extern "C" fn sys_alloc_frames(pages: u32) -> u64 {
    frame::alloc_frames(pages as usize)
        .map(|p| p as u64)
        .unwrap_or(0)
}

/// `free_frames(base, n)`.
unsafe extern "C" fn sys_free_frames(base: u64, pages: u32) -> i32 {
    for i in 0..pages as usize {
        frame::free_frame(base as usize + i * crate::mem::PAGE_SIZE);
    }
    0
}

// ── Logging syscall ───────────────────────────────────────────────────────────

/// `log(level, msg)` — kernel log line prefixed with the sender's name.
unsafe extern "C" fn sys_log(level: u32, msg: *const u8) {
    let sched = scheduler();
    let name = sched.current_name();
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

// ── The syscall table handed to servers ───────────────────────────────────────

static SYSCALL_TABLE: crate::sched::SyscallTable = crate::sched::SyscallTable {
    send: sys_send,
    receive: sys_receive,
    spawn: sys_spawn,
    who: sys_who,
    exit_task: sys_exit_task,
    exit: sys_exit,
    alloc_frames: sys_alloc_frames,
    free_frames: sys_free_frames,
    log: sys_log,
};

/// Pointer to the kernel's syscall table, given to every server at boot.
pub fn table_ptr() -> *const crate::sched::SyscallTable {
    core::ptr::addr_of!(SYSCALL_TABLE)
}
