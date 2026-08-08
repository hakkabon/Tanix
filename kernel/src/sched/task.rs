#![allow(dead_code)]
//! Task control block and priority scheduler — Phase 7.
//!
//! Each task has:
//!   • A saved AArch64 register context (callee-saved + SP + PC).
//!   • A task state (Ready / Running / Blocked / Zombie).
//!   • A priority (0 = highest, 255 = lowest) — Phase 7.
//!   • An identity (TaskId + name string).
//!   • IPC state (receive wait, pending senders, boot info) — Phase 4.
//!   • An IRQ-wait state (SYS_WAIT_IRQ) — Phase 7.
//!
//! The scheduler is a fixed-size static array — no heap allocation.  The
//! run queue is scanned for the highest-priority runnable task, with
//! round-robin rotation among equal priorities (Phase 7).  Preemption
//! happens on every timer tick (at EL0) and on every syscall return.

use super::{BootInfo, Message, PendingSend, TaskId, TaskState, M_ANY};

// ── Saved register context ────────────────────────────────────────────────────

/// SPSR_EL1 value for kernel contexts: EL1h, all exceptions masked (matches
/// the PSTATE the kernel actually runs with — the GIC and timer are set up
/// but no interrupt source is ever enabled, so DAIF stays masked).
pub const SPSR_KERNEL: u64 = 0x3C5;

/// SPSR_EL1 value for EL0 tasks: EL0t, all exceptions masked (same DAIF
/// policy as the kernel itself).
pub const SPSR_USER: u64 = 0x3C0;

/// Saved register context.
///
/// Layout must match the `context_switch` assembly stub in `switch.s`.
#[repr(C)]
pub struct Context {
    /// x19 – x28 (10 callee-saved GPRs per the AArch64 ABI).
    pub x19_to_x28: [u64; 10],
    /// Frame pointer (x29).
    pub fp: u64,
    /// Link register (x30) — holds the resume PC (loaded into ELR_EL1).
    pub lr: u64,
    /// Kernel stack pointer (SP_EL1).
    pub sp: u64,
    /// User stack pointer (SP_EL0) — meaningful only for EL0 tasks.
    pub sp_el0: u64,
    /// SPSR_EL1 to restore: EL1h for kernel contexts, EL0t for user tasks.
    pub spsr: u64,
    /// TTBR0_EL1 — the task's page table (kernel table for kernel contexts).
    pub ttbr0: u64,
}

impl Context {
    pub const fn zeroed() -> Self {
        Self {
            x19_to_x28: [0u64; 10],
            fp: 0,
            lr: 0,
            sp: 0,
            sp_el0: 0,
            spsr: 0,
            ttbr0: 0,
        }
    }

    /// Initialise a context that will start executing `entry_fn` at EL1h
    /// with a stack top at `stack_top` (used for kernel-side tasks and the
    /// Phase-3 guest, which runs in the kernel's address space).
    pub fn new(entry_fn: usize, stack_top: usize) -> Self {
        let mut ctx = Self::zeroed();
        ctx.lr = entry_fn as u64; // `eret` in context_switch jumps here
        ctx.sp = stack_top as u64;
        ctx.spsr = SPSR_KERNEL;
        ctx.ttbr0 = crate::mem::page_table::kernel_l0_phys() as u64;
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

    /// Initialise an **EL0** context: enters `entry_fn` at EL0t with the
    /// user stack `sp_el0`, the task's own address space `ttbr0` (physical
    /// address of its L0 page table), the kernel stack `kernel_stack_top`
    /// for its EL1 side, and the boot-info pointer in x19.
    pub fn new_user(
        entry_fn: usize,
        sp_el0: usize,
        ttbr0: u64,
        kernel_stack_top: usize,
        boot_info: usize,
    ) -> Self {
        let mut ctx = Self::zeroed();
        ctx.lr = entry_fn as u64;
        ctx.sp = kernel_stack_top as u64;
        ctx.sp_el0 = sp_el0 as u64;
        ctx.spsr = SPSR_USER;
        ctx.ttbr0 = ttbr0;
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
    /// Physical address of this task's L0 page table (0 = kernel table —
    /// kernel contexts and the Phase-3 guest share the kernel's address
    /// space).  Used to map alloc'd frames into EL0-accessible memory.
    pub ttbr0: u64,
    /// Scheduling priority — 0 is highest, 255 is lowest (Phase 7).
    pub priority: u8,
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
    // ── Phase 7 IRQ state ────────────────────────────────────────────────────
    /// The IRQ this task is currently blocked in `SYS_WAIT_IRQ` on
    /// (None = not waiting).  Only the current task can be in a wait loop
    /// (the kernel runs on its stack with interrupts unmasked).
    pub irq_wait: Option<u32>,
}

impl Task {
    pub const fn idle() -> Self {
        Self {
            id: TaskId(0),
            state: TaskState::Running,
            ctx: Context::zeroed(),
            name: *b"idle\0\0\0\0\0\0\0\0\0\0\0\0",
            ttbr0: 0,
            priority: super::PRIO_IDLE,
            recv_blocked: false,
            recv_filter: M_ANY,
            recv_buf: core::ptr::null_mut(),
            pending_senders: [None, None],
            boot: BootInfo { task_id: 0 },
            irq_wait: None,
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
            ttbr0: 0,
            priority: super::PRIO_NORMAL,
            recv_blocked: false,
            recv_filter: M_ANY,
            recv_buf: core::ptr::null_mut(),
            pending_senders: [None, None],
            boot: BootInfo { task_id: 0 },
            irq_wait: None,
        }
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(TASK_NAME_LEN);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

/// Maximum number of concurrent tasks (including idle).
///
/// Phase 4/5/7: idle + init + pm + mem + dev + worker + display + ui-demo
/// + hog = 9 slots in use; headroom for a couple more.
pub const MAX_TASKS: usize = 12;

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
                None, None, None, None, None, None, None, None, None, None, None,
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

    /// Spawn an **EL0** task with its own address space (Phase 6).
    ///
    /// `ttbr0` is the physical address of the task's L0 page table,
    /// `sp_el0` the user stack pointer, `kernel_stack_top` the top of the
    /// EL1-side kernel stack, and `boot` is copied into the memory at
    /// `boot_info` (an EL0-readable page in the task's region, whose
    /// address is preloaded into x19).
    pub fn spawn_user(
        &mut self,
        name: &str,
        entry: usize,
        ttbr0: u64,
        sp_el0: usize,
        kernel_stack_top: usize,
        boot_info: usize,
        boot: BootInfo,
    ) -> Option<TaskId> {
        let slot = self.tasks.iter_mut().skip(1).find(|s| s.is_none())?;
        let id = TaskId(self.next_id);
        self.next_id += 1;
        let t = slot.insert(Task::new(id, name, 0, 0));
        t.boot = boot;
        t.boot.task_id = id.0;
        unsafe {
            core::ptr::write_volatile(boot_info as *mut BootInfo, t.boot);
        }
        t.ttbr0 = ttbr0;
        t.ctx = Context::new_user(entry, sp_el0, ttbr0, kernel_stack_top, boot_info);
        log::debug!(
            "spawned EL0 task {:?} '{}' entry={:#x} ttbr0={:#x} sp_el0={:#x} boot={:#x}",
            id, name, entry, ttbr0, sp_el0, boot_info
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

    /// Pick the best Ready/Running task (Phase 7 priorities).
    /// Returns the index of the chosen task.
    ///
    ///  • Highest priority wins (lowest numeric value).
    ///  • Equal priorities rotate round-robin: ties are broken by forward
    ///    distance from the current slot, so the same task is never picked
    ///    twice in a row while others are runnable.
    ///  • The idle slot (index 0) is a fallback only: it is chosen when no
    ///    other task is runnable (e.g. every server is blocked in IPC), so
    ///    `enter()` can resume the kernel boot context.
    pub fn pick_next(&mut self) -> usize {
        let n = self.tasks.len();
        let cur = self.current;
        let mut best: Option<(u8, usize)> = None; // (priority, slot)
        for idx in 1..n {
            if let Some(ref t) = self.tasks[idx] {
                if t.state == TaskState::Ready || t.state == TaskState::Running {
                    let forward = (idx + n - cur - 1) % n;
                    match best {
                        None => best = Some((t.priority, idx)),
                        Some((bp, bi)) => {
                            let bfwd = (bi + n - cur - 1) % n;
                            if t.priority < bp || (t.priority == bp && forward < bfwd) {
                                best = Some((t.priority, idx));
                            }
                        }
                    }
                }
            }
        }
        best.map(|(_, idx)| idx).unwrap_or(0)
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

    /// Change a task's scheduling priority (0 = highest, 255 = lowest).
    pub fn set_priority(&mut self, id: TaskId, priority: u8) {
        if let Some(t) = self.task_by_id_mut(id) {
            t.priority = priority;
        }
    }

    /// (Priority, name) of the task in slot `idx`.
    pub fn priority_and_name(&self, idx: usize) -> Option<(u8, &str)> {
        self.tasks[idx].as_ref().map(|t| (t.priority, t.name_str()))
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

/// Spawn an EL0 server task in its own address space (Phase 6).
pub fn spawn_server_user(
    name: &str,
    entry: usize,
    ttbr0: u64,
    sp_el0: usize,
    kernel_stack_top: usize,
    boot_info: usize,
    boot: BootInfo,
) -> Option<TaskId> {
    unsafe {
        (*core::ptr::addr_of_mut!(SCHEDULER))
            .spawn_user(name, entry, ttbr0, sp_el0, kernel_stack_top, boot_info, boot)
    }
}

/// Id of the currently running task (0 = kernel boot context).
pub fn current_id() -> TaskId {
    unsafe { (*core::ptr::addr_of_mut!(SCHEDULER)).current_id() }
}

/// Physical address of the current task's L0 page table (0 = kernel table).
///
/// EL0 tasks carry their own address space; kernel contexts and the guest
/// share the kernel's identity map.  Used by the memory syscalls to map
/// allocated frames into the caller's address space.
pub fn current_ttbr0() -> u64 {
    unsafe {
        let sched = &*core::ptr::addr_of!(SCHEDULER);
        sched.tasks[sched.current]
            .as_ref()
            .map(|t| t.ttbr0)
            .unwrap_or(0)
    }
}

/// Change a task's scheduling priority (0 = highest, 255 = lowest).
pub fn set_task_priority(id: TaskId, priority: u8) {
    unsafe {
        (*core::ptr::addr_of_mut!(SCHEDULER)).set_priority(id, priority);
    }
}

/// Core of every preemptive reschedule: if the current task is runnable
/// and a better task exists, switch to it.  Returns `true` if a switch
/// happened (the caller is now a different task).
///
/// The current task's state must be `Running` for it to be rotated back
/// into the run queue; blocked tasks are left alone.
///
/// `strict` selects the eligibility rule:
///   • `false` (timer tick at EL0): any better pick — including an
///     equal-priority RR rotation.  Safe: the current task is at EL0 and
///     will reach a blocking point / the next tick naturally.
///   • `true` (syscall tail, yield): only a *strictly higher*-priority
///     task.  Same-priority tasks whose syscall tails are suspended must
///     not switch into each other — that would ping-pong forever between
///     two tails with neither ever returning to EL0.  Equal-priority
///     rotation happens on ticks instead.
///
/// # Safety
/// Must be called from EL1 with interrupts masked (the caller owns the
/// stack that the preempted context is saved on).
unsafe fn switch_best(strict: bool) -> bool {
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let cur = sched.current_idx();
    if cur == 0 {
        return false; // boot context — never preempt it
    }
    let cur_running = sched.tasks[cur]
        .as_ref()
        .map(|t| t.state == TaskState::Running)
        .unwrap_or(false);
    if !cur_running {
        return false;
    }
    let next = sched.pick_next();
    if next == cur {
        return false;
    }
    let cur_prio = sched.priority_and_name(cur).map(|(p, _)| p).unwrap_or(255);
    let next_prio = sched
        .priority_and_name(next)
        .map(|(p, _)| p)
        .unwrap_or(255);
    if strict && next_prio >= cur_prio {
        return false;
    }
    log::trace!(
        "sched: preempt '{}'(p{}) -> '{}'(p{})",
        sched.priority_and_name(cur).map(|(_, n)| n).unwrap_or("?"),
        cur_prio,
        sched.priority_and_name(next).map(|(_, n)| n).unwrap_or("?"),
        next_prio,
    );
    let from = sched.ctx_ptr(cur);
    let to = sched.ctx_ptr(next);
    sched.set_state(cur, TaskState::Ready);
    sched.set_state(next, TaskState::Running);
    sched.set_current(next);
    context_switch(from, to);
    true
}

/// Preemption entry point for the timer-tick IRQ handler (Phase 7).
///
/// `from_el0` must be `true` only when the tick interrupted an EL0 task —
/// ticks landing in the kernel (e.g. inside the `SYS_WAIT_IRQ` wait loop,
/// which runs with IRQs unmasked) never preempt; they just record the
/// tick.  When the preempted task is later resumed, it continues inside
/// `irq_handler` after the switch and returns to its interrupted EL0 point
/// via the IRQ frame's saved ELR/SPSR.
///
/// # Safety
/// Called from `irq_handler` (interrupts masked by hardware).
pub unsafe fn tick_preempt(from_el0: bool) {
    if !from_el0 {
        return;
    }
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let cur = sched.current_idx();
    if cur == 0 {
        return; // tick landed in the boot context — nothing to preempt
    }
    let _ = switch_best(false);
}

/// Cooperative `SYS_YIELD` — hand the CPU to a strictly higher-priority
/// task, if any is runnable (equal-priority rotation happens on ticks).
///
/// # Safety
/// Called from the syscall dispatcher (EL1, interrupts masked).
pub unsafe fn yield_cpu() {
    let _ = switch_best(true);
}

/// Preemption check run at the end of every syscall: if a strictly
/// higher-priority task became runnable (woken by this syscall), switch to
/// it before returning to EL0.
///
/// # Safety
/// Called from the syscall dispatcher (EL1, interrupts masked).
pub unsafe fn reschedule() {
    let _ = switch_best(true);
}

/// Name of the currently running task.
pub fn current_name() -> &'static str {    // SAFETY: the scheduler's static task table lives for the whole program.
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
///
/// Phase 6: if the task ran at EL0 with its own tables, they are freed so
/// their frames return to the allocator.
pub fn kill_task(id: TaskId) -> bool {
    unsafe {
        let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
        let current = sched.current_id();
        let t = match sched.task_by_id_mut(id) {
            Some(t) => t,
            None => return false,
        };
        let victim_ttbr0 = t.ttbr0;
        let victim_is_current = id == current;
        t.state = TaskState::Zombie;
        t.recv_blocked = false;
        // Free the victim's address space unless it is ourselves (its
        // tables stay active until the next switch; freeing them now would
        // be unsafe — self-kill leaks them instead, like exit_current).
        if victim_ttbr0 != 0 && !victim_is_current {
            t.ttbr0 = 0;
            crate::mem::page_table::free_task_tables(victim_ttbr0 as usize);
        }
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
