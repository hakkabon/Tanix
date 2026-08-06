#![allow(dead_code)]
//! Task control block and round-robin scheduler.
//!
//! Each task has:
//!   • A saved AArch64 register context (callee-saved + SP + PC).
//!   • A task state (Ready / Running / Blocked / Zombie).
//!   • An identity (TaskId + name string).
//!
//! The scheduler is a simple round-robin over a fixed-size static array —
//! no heap allocation required.  This is sufficient for Phase 2 where we
//! have at most ~4 tasks (kernel idle + init server + 1-2 VM workers).

use super::{TaskId, TaskState};

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
}

// ── Task control block ────────────────────────────────────────────────────────

/// Maximum name length (inline, no allocation).
pub const TASK_NAME_LEN: usize = 16;

pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub ctx: Context,
    pub name: [u8; TASK_NAME_LEN],
}

impl Task {
    pub const fn idle() -> Self {
        Self {
            id: TaskId(0),
            state: TaskState::Running,
            ctx: Context::zeroed(),
            name: *b"idle\0\0\0\0\0\0\0\0\0\0\0\0",
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
        }
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

    /// Pick the next Ready task (round-robin, skipping Blocked/Zombie).
    /// Returns the index of the chosen task.
    pub fn pick_next(&mut self) -> usize {
        let n = self.tasks.len();
        let start = (self.current + 1) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            if let Some(ref t) = self.tasks[idx] {
                if t.state == TaskState::Ready || t.state == TaskState::Running {
                    return idx;
                }
            }
        }
        0 // fall back to idle
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

static mut SCHEDULER: Scheduler = Scheduler::new();

pub fn spawn_task(name: &str, entry: usize, stack_top: usize) -> Option<TaskId> {
    unsafe { (*core::ptr::addr_of_mut!(SCHEDULER)).spawn(name, entry, stack_top) }
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
    fn context_switch(from: *mut Context, to: *const Context);
}
