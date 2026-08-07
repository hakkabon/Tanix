#![allow(dead_code)]
//! Task control block and round-robin scheduler.
//!
//! Each task has:
//!   • A saved AArch64 register context (callee-saved + SP + PC).
//!   • A task state (Ready / Running / Blocked / Zombie).
//!   • An identity (TaskId + name string).
//!   • IPC state (receive wait, pending senders, boot info) — Phase 4.
//!
//! The scheduler is a simple round-robin over a fixed-size static array —
//! no heap allocation required.  This is sufficient for Phase 2 where we
//! have at most ~4 tasks (kernel idle + init server + 1-2 VM workers).

use super::{BootInfo, Message, PendingSend, TaskId, TaskState, M_ANY};

// ── Saved register context ────────────────────────────────────────────────────

/// Callee-saved general-purpose registers + SP + PC (ELR_EL1 equivalent).
///
/// Layout must match the `context_switch` assembly stub in `switch.s`.
#[repr(C)]
pub struct Context {
    /// x19 – x28 (10 callee-saved GPRs per the AArch64 ABI).
    pub x19_to_x28: [u64; 10],
    /// Frame pointer (x29).
    pub fp: u64,
    /// Link register (x30) — holds the return address / resume PC.
    pub lr: u64,
    /// Stack pointer.
    pub sp: u64,
}

impl Context {
    pub const fn zeroed() -> Self {
        Self {
            x19_to_x28: [0u64; 10],
            fp: 0,
            lr: 0,
            sp: 0,
        }
    }

    /// Initialise a context that will start executing `entry_fn` with
    /// a stack top at `stack_top`.
    pub fn new(entry_fn: usize, stack_top: usize) -> Self {
        let mut ctx = Self::zeroed();
        ctx.lr = entry_fn as u64; // `ret` in context_switch jumps here
        ctx.sp = stack_top as u64;
        ctx
    }

    /// Like `new`, but also preloads x19 with `boot_info`.
    ///
    /// x19 is callee-saved: it survives `context_switch` and is preserved by
    /// the C ABI, so the task's entry function can read it as its boot
    /// argument (server binaries do exactly that in `_start`).
    pub fn with_boot(entry_fn: usize, stack_top: usize, boot_info: usize) -> Self {
        let mut ctx = Self::new(entry_fn, stack_top);
        ctx.x19_to_x28[0] = boot_info as u64;
        ctx
    }
}

// ── Task control block ────────────────────────────────────────────────────────

/// Maximum name length (inline, no allocation).
pub const TASK_NAME_LEN: usize = 16;

/// Maximum number of senders blocked on one task at a time.
pub const MAX_PENDING_SENDERS: usize = 2;

pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub ctx: Context,
    pub name: [u8; TASK_NAME_LEN],
    // ── Phase 4 IPC state ────────────────────────────────────────────────────
    /// True while this task is blocked in `receive`.
    pub recv_blocked: bool,
    /// Sender filter of the pending `receive` (M_ANY = any sender).
    pub recv_filter: i32,
    /// Buffer the pending `receive` wants its message copied into.
    pub recv_buf: *mut Message,
    /// Senders blocked because their `send` could not rendezvous yet.
    pub pending_senders: [Option<PendingSend>; MAX_PENDING_SENDERS],
    /// Boot info handed to the task at spawn (x19 at first entry).
    pub boot: BootInfo,
}

impl Task {
    pub const fn idle() -> Self {
        Self {
            id: TaskId(0),
            state: TaskState::Running,
            ctx: Context::zeroed(),
            name: *b"idle\0\0\0\0\0\0\0\0\0\0\0\0",
            recv_blocked: false,
            recv_filter: M_ANY,
            recv_buf: core::ptr::null_mut(),
            pending_senders: [None, None],
            boot: BootInfo {
                syscalls: core::ptr::null(),
                task_id: 0,
            },
        }
    }

    pub fn new(id: TaskId, name: &str, entry: usize, stack_top: usize) -> Self {
        let mut name_buf = [0u8; TASK_NAME_LEN];
        let bytes = name.as_bytes();
        let copy_len = bytes.len().min(TASK_NAME_LEN);
        name_buf[..copy_len].copy_from_slice(&bytes[..copy_len]);
        Self {
            id,
            state: TaskState::Ready,
            ctx: Context::new(entry, stack_top),
            name: name_buf,
            recv_blocked: false,
            recv_filter: M_ANY,
            recv_buf: core::ptr::null_mut(),
            pending_senders: [None, None],
            boot: BootInfo {
                syscalls: core::ptr::null(),
                task_id: 0,
            },
        }
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(TASK_NAME_LEN);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

/// Maximum number of concurrent tasks (including idle).
pub const MAX_TASKS: usize = 8;

pub struct Scheduler {
    tasks: [Option<Task>; MAX_TASKS],
    current: usize, // index into `tasks`
    next_id: u32,
}

impl Scheduler {
    pub const fn new() -> Self {
        // Array of Option<Task> — manual init because Task is not Copy.
        Self {
            tasks: [
                Some(Task::idle()), // slot 0 = idle task
                None, None, None, None, None, None, None,
            ],
            current: 0,
            next_id: 1,
        }
    }

    /// Spawn a new task.  Returns its `TaskId`.
    pub fn spawn(&mut self, name: &str, entry: usize, stack_top: usize) -> Option<TaskId> {
        let slot = self.tasks.iter_mut().skip(1).find(|s| s.is_none())?;
        let id = TaskId(self.next_id);
        self.next_id += 1;
        *slot = Some(Task::new(id, name, entry, stack_top));
        log::debug!("spawned task {:?} entry={:#x}", id, entry);
        Some(id)
    }

    /// Spawn a new task with a boot-info pointer preloaded into x19.
    /// Returns its `TaskId`.
    pub fn spawn_with_boot(
        &mut self,
        name: &str,
        entry: usize,
        stack_top: usize,
        boot: BootInfo,
    ) -> Option<TaskId> {
        let slot = self.tasks.iter_mut().skip(1).find(|s| s.is_none())?;
        let id = TaskId(self.next_id);
        self.next_id += 1;
        let t = slot.insert(Task::new(id, name, entry, stack_top));
        t.boot = boot;
        t.boot.task_id = id.0;
        let boot_ptr = core::ptr::addr_of!(t.boot);
        t.ctx = Context::with_boot(entry, stack_top, boot_ptr as usize);
        log::debug!(
            "spawned task {:?} '{}' entry={:#x} boot={:#x}",
            id, name, entry, boot_ptr as usize
        );
        Some(id)
    }

    /// Look up a task by id.
    pub fn task_by_id(&self, id: TaskId) -> Option<&Task> {
        self.tasks
            .iter()
            .filter_map(|s| s.as_ref())
            .find(|t| t.id == id)
    }

    pub fn task_by_id_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|t| t.id == id)
    }

    /// Task at a scheduler slot index.
    pub fn task_at(&self, idx: usize) -> Option<&Task> {
        self.tasks[idx].as_ref()
    }

    pub fn task_at_mut(&mut self, idx: usize) -> Option<&mut Task> {
        self.tasks[idx].as_mut()
    }

    /// Index of the task with the given id.
    pub fn task_idx(&self, id: TaskId) -> Option<usize> {
        self.tasks
            .iter()
            .position(|s| s.as_ref().map(|t| t.id == id).unwrap_or(false))
    }

    /// All task slots (Some = occupied).
    pub fn task_slots(&self) -> &[Option<Task>] {
        &self.tasks
    }

    /// Name of the task that is currently running.
    pub fn current_name(&self) -> &str {
        self.tasks[self.current]
            .as_ref()
            .map(|t| t.name_str())
            .unwrap_or("?")
    }

    pub fn current_id(&self) -> TaskId {
        self.tasks[self.current]
            .as_ref()
            .map(|t| t.id)
            .unwrap_or(TaskId(0))
    }

    /// Pick the next Ready task (round-robin, skipping Blocked/Zombie).
    /// Returns the index of the chosen task.
    ///
    /// The idle slot (index 0) is a fallback only: it is chosen when no
    /// other task is runnable (e.g. every server is blocked in IPC), so
    /// `enter()` can resume the kernel boot context.
    pub fn pick_next(&mut self) -> usize {
        let n = self.tasks.len();
        let start = (self.current + 1) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            if idx == 0 {
                continue;
            }
            if let Some(ref t) = self.tasks[idx] {
                if t.state == TaskState::Ready || t.state == TaskState::Running {
                    return idx;
                }
            }
        }
        0 // nothing runnable — idle
    }

    /// Return a raw pointer to task `idx`'s context.
    pub fn ctx_ptr(&mut self, idx: usize) -> *mut Context {
        self.tasks[idx]
            .as_mut()
            .map(|t| &mut t.ctx as *mut Context)
            .unwrap_or(core::ptr::null_mut())
    }

    pub fn current_idx(&self) -> usize {
        self.current
    }

    pub fn set_current(&mut self, idx: usize) {
        self.current = idx;
    }

    pub fn set_state(&mut self, idx: usize, state: TaskState) {
        if let Some(ref mut t) = self.tasks[idx] {
            t.state = state;
        }
    }
}

// ── Global scheduler instance ─────────────────────────────────────────────────

pub(crate) static mut SCHEDULER: Scheduler = Scheduler::new();

pub(crate) fn scheduler() -> &'static mut Scheduler {
    unsafe { &mut *core::ptr::addr_of_mut!(SCHEDULER) }
}

pub fn spawn_task(name: &str, entry: usize, stack_top: usize) -> Option<TaskId> {
    unsafe { (*core::ptr::addr_of_mut!(SCHEDULER)).spawn(name, entry, stack_top) }
}

/// Spawn a server task whose x19 is preloaded with `boot`.
pub fn spawn_server(name: &str, entry: usize, stack_top: usize, boot: BootInfo) -> Option<TaskId> {
    unsafe {
        (*core::ptr::addr_of_mut!(SCHEDULER))
            .spawn_with_boot(name, entry, stack_top, boot)
    }
}

/// Id of the currently running task (0 = kernel boot context).
pub fn current_id() -> TaskId {
    unsafe { (*core::ptr::addr_of_mut!(SCHEDULER)).current_id() }
}

/// Name of the currently running task.
pub fn current_name() -> &'static str {
    // SAFETY: the scheduler's static task table lives for the whole program.
    unsafe {
        let sched = &*core::ptr::addr_of!(SCHEDULER);
        let name = sched.current_name();
        // Tasks store their names inline in the static table, so the slice
        // is 'static in practice.  Re-slice from the static to keep lifetimes
        // honest with the compiler.
        let ptr = name.as_ptr();
        let len = name.len();
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len))
    }
}

/// Look up a task's name by id.
pub fn task_name(id: TaskId) -> Option<&'static str> {
    unsafe {
        let sched = &*core::ptr::addr_of!(SCHEDULER);
        let t = sched.task_by_id(id)?;
        Some(core::str::from_utf8_unchecked(
            core::ptr::addr_of!(t.name).as_ref().unwrap(),
        ))
    }
}

/// Mark a task (by id) as zombie.  Returns `false` if it does not exist.
pub fn kill_task(id: TaskId) -> bool {
    unsafe {
        let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
        let t = match sched.task_by_id_mut(id) {
            Some(t) => t,
            None => return false,
        };
        t.state = TaskState::Zombie;
        t.recv_blocked = false;
        true
    }
}

/// Terminate the calling task: mark it zombie and switch away.
pub fn exit_current() -> ! {
    unsafe {
        let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
        let idx = sched.current_idx();
        sched.set_state(idx, TaskState::Zombie);
        log::debug!("task {:?} '{}' exited", sched.current_id(), sched.current_name());
        // Switch to the next runnable task; nothing must return here.
        loop {
            let next = sched.pick_next();
            if next == idx {
                // Nothing else runnable — the boot context (idle slot) is
                // the only place left to go; it resumed only once all
                // servers finished, so it halts.
                let from = sched.ctx_ptr(idx);
                let to = sched.ctx_ptr(0);
                sched.set_current(0);
                context_switch(from, to);
            } else {
                let from = sched.ctx_ptr(idx);
                let to = sched.ctx_ptr(next);
                sched.set_state(next, TaskState::Running);
                sched.set_current(next);
                context_switch(from, to);
            }
        }
    }
}

/// Enter the scheduler from the kernel boot context (called from `kmain`).
///
/// The current (kernel) context is parked in the idle slot (task 0); the
/// scheduler then runs server tasks.  When every task is blocked or zombie,
/// round-robin falls back to the idle slot and this function returns,
/// resuming `kmain` after all servers have finished.
///
/// # Safety
/// Must be called exactly once, after at least one server task was spawned.
pub unsafe fn enter() {
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);

    let next = sched.pick_next();
    if next == 0 {
        return; // nothing spawned — nothing to run
    }

    let from = sched.ctx_ptr(0); // idle slot = our boot context
    let to = sched.ctx_ptr(next);
    sched.set_state(next, TaskState::Running);
    sched.set_current(next);

    context_switch(from, to);

    // Resumed only when every server task blocked/zombie → kmain continues.
    log::debug!("scheduler: back in kernel boot context");
}

/// Perform a round-robin context switch.
///
/// # Safety
/// Must be called with interrupts disabled.
pub unsafe fn schedule() {
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);

    let current_idx = sched.current_idx();
    let next_idx = sched.pick_next();

    if current_idx == next_idx {
        return;
    }

    let from = sched.ctx_ptr(current_idx);
    let to   = sched.ctx_ptr(next_idx);

    if from.is_null() || to.is_null() {
        return;
    }

    if sched.tasks[current_idx]
        .as_ref()
        .map(|t| t.state == TaskState::Running)
        .unwrap_or(false)
    {
        sched.set_state(current_idx, TaskState::Ready);
    }
    sched.set_state(next_idx, TaskState::Running);
    sched.set_current(next_idx);

    context_switch(from, to);
}

extern "C" {
    /// Defined in `switch.s`.
    ///
    /// Saves the current task's callee-saved registers / SP / LR into `from`
    /// and restores `to`, then returns into the restored context.
    ///
    /// Only touches x0, x1, x9, x19–x30 and SP — all other registers
    /// (including x4–x8, x10–x18) survive the switch, which the VM manager
    /// relies on to pass boot arguments to a guest.
    pub(crate) fn context_switch(from: *mut Context, to: *const Context);
}
