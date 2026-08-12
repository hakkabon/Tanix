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
//!
//! Phase 11 (SMP): there is ONE global runqueue guarded by `SCHED_LOCK`.
//! Every scheduling entry point (tick, syscall tail, blocking switch,
//! idle loop) acquires the lock, mutates task states, picks the next task
//! and then calls `context_switch_unlock`, whose assembly releases the
//! lock *between* saving the current context and restoring the next one —
//! the critical section therefore spans exactly [lock → pick → save],
//! and the resumed task never has to (and never may) unlock.  Tasks
//! migrate freely between cores; the lock guarantees at most one core
//! executes a given task (a Running task is invisible to other picks,
//! which consider only Ready tasks).  Each core tracks its own current
//! slot in `crate::smp`; CPU 0's idle fallback is the boot context
//! (slot 0), each secondary has a dedicated idle task slot.

use super::{BootInfo, Message, PendingSend, StagedSend, TaskId, TaskState, M_ANY};
use crate::sync::SpinLock;

/// Guards the entire scheduler state (task table, states, `woken`).
///
/// All scheduler entry points hold it; the switch stubs release it
/// mid-`context_switch`.  Interrupts are masked on every CPU while the
/// lock can be held, so a holder is never preempted.
pub(crate) static SCHED_LOCK: SpinLock = SpinLock::new();

pub(crate) fn sched_lock() -> &'static SpinLock {
    &SCHED_LOCK
}

// ── Saved register context ────────────────────────────────────────────────────

/// SPSR_EL1 value for kernel contexts: EL1h, all exceptions masked (matches
/// the PSTATE the kernel actually runs with — the GIC and timer are set up
/// but no interrupt source is ever enabled, so DAIF stays masked).
pub const SPSR_KERNEL: u64 = 0x3C5;

/// SPSR_EL1 value for EL0 tasks: EL0t with IRQs **unmasked** — the timer
/// tick must be able to interrupt user code (slot 9, `from_el0=1`) or the
/// lower-priority tasks would starve: ticks would only ever land inside
/// the kernel's `SYS_WAIT_IRQ` wait loop, where no preemption is allowed.
pub const SPSR_USER: u64 = 0x0;

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
    /// CPU affinity (Phase 11): `None` = any core may run this task;
    /// `Some(cpu)` = only core `cpu` (used for the per-CPU idle tasks).
    pub cpu_affinity: Option<usize>,
    // ── Phase 4 IPC state ────────────────────────────────────────────────────
    /// True while this task is blocked in `receive`.
    pub recv_blocked: bool,
    /// Sender filter of the pending `receive` (M_ANY = any sender).
    pub recv_filter: i32,
    /// Buffer the pending `receive` wants its message copied into.
    pub recv_buf: *mut Message,
    /// Senders blocked because their `send` could not rendezvous yet.
    pub pending_senders: [Option<PendingSend>; MAX_PENDING_SENDERS],
    /// Task we are blocked in `send` on because its `pending_senders`
    /// queue was full (None = not queue-waiting).  A send parked in this
    /// state never drops the message: the receiver's `receive` delivers
    /// the staged message directly the moment its filter matches and wakes
    /// us (see `sys_receive`); the resumed send returns success.
    pub send_full_wait: Option<StagedSend>,
    /// Boot info handed to the task at spawn (x19 at first entry).
    pub boot: BootInfo,
    // ── Phase 7 IRQ state ────────────────────────────────────────────────────
    /// The IRQ this task is currently blocked in `SYS_WAIT_IRQ` on
    /// (None = not waiting).  Only the current task can be in a wait loop
    /// (the kernel runs on its stack with interrupts unmasked).
    pub irq_wait: Option<u32>,
    // ── Phase 8 sleep state ──────────────────────────────────────────────────
    /// Tick deadline for `SYS_SLEEP`; 0 = not sleeping.  Set while the task
    /// is Blocked in `sys_sleep`; the timer-tick handler wakes the task once
    /// `timer::ticks()` passes it.
    pub sleep_deadline: u64,
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
            cpu_affinity: Some(0), // slot 0 = CPU 0's boot context only
            recv_blocked: false,
            recv_filter: M_ANY,
            recv_buf: core::ptr::null_mut(),
            pending_senders: [None, None],
            send_full_wait: None,
            boot: BootInfo { task_id: 0, machine: 0 },
            irq_wait: None,
            sleep_deadline: 0,
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
            cpu_affinity: None,
            recv_blocked: false,
            recv_filter: M_ANY,
            recv_buf: core::ptr::null_mut(),
            pending_senders: [None, None],
            send_full_wait: None,
            boot: BootInfo { task_id: 0, machine: 0 },
            irq_wait: None,
            sleep_deadline: 0,
        }
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(TASK_NAME_LEN);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

/// Maximum number of concurrent tasks (including idle slots).
///
/// Phase 8: idle + init + pm + mem + dev + worker + display + ui-demo +
/// wm + counter + clock + hog = 12 slots in use; headroom for more.
///
/// Phase 11 (SMP): slots `IDLE_SLOT_BASE .. IDLE_SLOT_BASE + MAX_CPUS - 2`
/// are reserved for the secondary CPUs' idle tasks.
pub const MAX_TASKS: usize = 32;

/// First slot reserved for per-CPU idle tasks (CPU n → `IDLE_SLOT_BASE + n - 1`).
pub const IDLE_SLOT_BASE: usize = 16;

pub struct Scheduler {
    tasks: [Option<Task>; MAX_TASKS],
    next_id: u32,
    /// Slot of the task most recently woken by the current syscall, if any.
    /// The syscall tail (`reschedule`) switches to it only when it is
    /// strictly higher priority than the caller — wakeup-driven preemption,
    /// so a busy high-priority task (wm) cannot starve the others.
    woken: Option<usize>,
}

impl Scheduler {
    pub const fn new() -> Self {
        // Array of Option<Task> — manual init because Task is not Copy.
        Self {
            tasks: [
                Some(Task::idle()), // slot 0 = boot context (CPU 0's idle)
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
                const { None },
            ],
            next_id: 1,
            woken: None,
        }
    }

    /// Record that the syscall currently executing has just woken the task
    /// in `slot`.
    pub fn set_woken(&mut self, slot: usize) {
        self.woken = Some(slot);
    }

    /// Take the woken-task record (consumed once, by the syscall tail).
    pub fn take_woken(&mut self) -> Option<usize> {
        self.woken.take()
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
    #[allow(clippy::too_many_arguments)]
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
        let slot_idx = (1..MAX_TASKS).find(|i| self.tasks[*i].is_none())?;
        let slot = &mut self.tasks[slot_idx];
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
        // The new task is ready and waiting: record it so the spawner's
        // syscall tail can hand over to it if it is higher priority.
        self.woken = Some(slot_idx);
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

    /// Name of the task that is currently running (on this CPU).
    pub fn current_name(&self) -> &str {
        self.tasks[crate::smp::current_idx()]
            .as_ref()
            .map(|t| t.name_str())
            .unwrap_or("?")
    }

    pub fn current_id(&self) -> TaskId {
        self.tasks[crate::smp::current_idx()]
            .as_ref()
            .map(|t| t.id)
            .unwrap_or(TaskId(0))
    }

    /// Create the idle task for secondary CPU `cpu` (Phase 11).  Returns
    /// its slot index.  Called by `smp::bring_up()` before the secondary
    /// is released, so the slot exists before any pick can find it.
    ///
    /// The context is left zeroed on purpose: the save side of the
    /// secondary's first `context_switch_unlock` fills it with the
    /// secondary boot context (exactly how slot 0 captures kmain).
    pub fn add_idle(&mut self, cpu: usize) -> usize {
        let idx = IDLE_SLOT_BASE + cpu - 1;
        debug_assert!(self.tasks[idx].is_none());
        let mut t = Task::idle();
        t.id = TaskId(0x4000_0000 + cpu as u32);
        t.cpu_affinity = Some(cpu);
        let mut name = *b"idle-cpu\0\0\0\0\0\0\0\0";
        name[8] = b'0' + cpu as u8; // "idle-cpu1"
        t.name = name;
        self.tasks[idx] = Some(t);
        log::info!("sched: idle slot {} reserved for CPU {}", idx, cpu);
        idx
    }

    /// Pick the best Ready task for CPU `cpu` (Phase 7 priorities).
    /// Returns the index of the chosen task.
    ///
    ///  • Only **Ready** tasks are candidates (Running tasks are owned by
    ///    whichever core set them Running — never visible here).
    ///  • Highest priority wins (lowest numeric value).
    ///  • Equal priorities rotate round-robin: ties are broken by forward
    ///    distance from `cpu`'s current slot, so the same task is never
    ///    picked twice in a row while others are runnable.
    ///  • Tasks with `cpu_affinity` for another core are skipped.
    ///  • The CPU's own idle slot is the fallback: chosen when no other
    ///    task is runnable (e.g. every server is blocked in IPC), so
    ///    `enter()` can resume the boot context (CPU 0) or a secondary's
    ///    idle loop can sleep in WFI.
    pub fn pick_next(&mut self, cpu: usize) -> usize {
        let n = self.tasks.len();
        let cur = crate::smp::current_idx();
        let mut best: Option<(u8, usize)> = None; // (priority, slot)
        for idx in 1..n {
            if let Some(ref t) = self.tasks[idx] {
                if let Some(aff) = t.cpu_affinity {
                    if aff != cpu {
                        continue;
                    }
                }
                if t.state == TaskState::Ready {
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
        best.map(|(_, idx)| idx).unwrap_or(crate::smp::idle_slot(cpu))
    }

    /// Round-robin pick for CPU `cpu`: the Ready slot with the smallest
    /// forward distance from the CPU's current slot, ignoring priorities —
    /// the tick uses this so every ready task gets its time slice even
    /// while a higher-priority task stays busy.  `exclude` (the preempted
    /// task) is skipped.  Returns the CPU's own idle slot only when
    /// nothing else is runnable.
    pub fn pick_next_rr(&mut self, cpu: usize, exclude: usize) -> usize {
        let n = self.tasks.len();
        let cur = crate::smp::current_idx();
        let mut best: Option<usize> = None;
        for idx in 1..n {
            if idx == exclude {
                continue;
            }
            if let Some(ref t) = self.tasks[idx] {
                if let Some(aff) = t.cpu_affinity {
                    if aff != cpu {
                        continue;
                    }
                }
                if t.state == TaskState::Ready {
                    let forward = (idx + n - cur - 1) % n;
                    match best {
                        None => best = Some(idx),
                        Some(bi) => {
                            if forward < (bi + n - cur - 1) % n {
                                best = Some(idx);
                            }
                        }
                    }
                }
            }
        }
        best.unwrap_or(crate::smp::idle_slot(cpu))
    }

    /// Return a raw pointer to task `idx`'s context.
    pub fn ctx_ptr(&mut self, idx: usize) -> *mut Context {
        self.tasks[idx]
            .as_mut()
            .map(|t| &mut t.ctx as *mut Context)
            .unwrap_or(core::ptr::null_mut())
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
    let lock = sched_lock();
    lock.lock();
    let r = unsafe { (*core::ptr::addr_of_mut!(SCHEDULER)).spawn(name, entry, stack_top) };
    lock.unlock();
    r
}

/// Spawn a server task whose x19 is preloaded with `boot`.
pub fn spawn_server(name: &str, entry: usize, stack_top: usize, boot: BootInfo) -> Option<TaskId> {
    let lock = sched_lock();
    lock.lock();
    let r = unsafe {
        (*core::ptr::addr_of_mut!(SCHEDULER)).spawn_with_boot(name, entry, stack_top, boot)
    };
    lock.unlock();
    r
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
    let lock = sched_lock();
    lock.lock();
    let r = unsafe {
        spawn_server_user_locked(name, entry, ttbr0, sp_el0, kernel_stack_top, boot_info, boot)
    };
    lock.unlock();
    r
}

/// Lock-held variant of `spawn_server_user` (Phase 11): the syscall
/// dispatcher runs the whole syscall under `SCHED_LOCK`, so its spawn path
/// must not re-acquire it.  Callers: `server::spawn_by_name_locked`.
pub(crate) unsafe fn spawn_server_user_locked(
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

/// Reserve the idle slot for secondary CPU `cpu` (Phase 11).  Called by
/// `smp::bring_up()` before the core is released via PSCI.
pub fn add_idle(cpu: usize) -> usize {
    let lock = sched_lock();
    lock.lock();
    let idx = unsafe { (*core::ptr::addr_of_mut!(SCHEDULER)).add_idle(cpu) };
    lock.unlock();
    idx
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
        sched.tasks[crate::smp::current_idx()]
            .as_ref()
            .map(|t| t.ttbr0)
            .unwrap_or(0)
    }
}

/// Change a task's scheduling priority (0 = highest, 255 = lowest).
pub fn set_task_priority(id: TaskId, priority: u8) {
    let lock = sched_lock();
    lock.lock();
    unsafe {
        (*core::ptr::addr_of_mut!(SCHEDULER)).set_priority(id, priority);
    }
    lock.unlock();
}

/// Lock-held variant of `set_task_priority` (Phase 11): the spawn path
/// inside the syscall dispatcher already holds `SCHED_LOCK`.
pub unsafe fn set_task_priority_locked(id: TaskId, priority: u8) {
    (*core::ptr::addr_of_mut!(SCHEDULER)).set_priority(id, priority);
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
/// # Safety / locking (Phase 11)
/// Must be called from EL1 with interrupts masked and `SCHED_LOCK`
/// **already held**.  A real switch releases the lock mid-`context_switch`
/// (`context_switch_unlock`); when no switch happens the lock stays held
/// and the caller unlocks.  On return, the lock is *never* held.
unsafe fn switch_best(strict: bool) -> bool {
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let cpu = crate::smp::cpu_index();
    let cur = crate::smp::current_idx();
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
    // Strict picks (yields, wakeup tails) may re-select the current task —
    // it stays eligible and wins ties.  A tick preemption must *rotate*:
    // the current task just consumed its whole quantum, so it is excluded
    // from the pick; the tick hands the slice to the next runnable slot in
    // round-robin order, otherwise a busy high-priority task would be
    // re-picked forever and the lower priorities would starve.
    let next = if strict {
        sched.pick_next(cpu)
    } else {
        sched.pick_next_rr(cpu, cur)
    };
    if next == cur || next == 0 {
        return false; // nothing else runnable — stay put (never idle-switch from a tick)
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
    crate::smp::set_current(next);
    context_switch_unlock(sched_lock(), from, to);
    true
}

/// Wake every task whose `SYS_SLEEP` deadline has passed.  Returns whether
/// any task was woken.  Called from the timer-tick handler (under
/// `SCHED_LOCK`); waking is a pure state change, the actual hand-off
/// happens at the next scheduling point (tick on EL0, syscall tail, or —
/// for an otherwise-idle core — an immediate switch in `tick_preempt` /
/// the per-CPU idle loop).
///
/// Phase 11: a woken sleeper has no affinity — if any secondary core is
/// parked in WFI, poke it so it picks the sleeper up instead of leaving it
/// to a (busy or sleeping) other core.
///
/// # Safety
/// Called from `irq_handler` with `SCHED_LOCK` held (interrupts masked).
pub unsafe fn wake_sleepers() -> bool {
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let now = crate::arch::aarch64::timer::ticks();
    let mut woke = false;
    for idx in 1..sched.tasks.len() {
        if let Some(t) = sched.task_at_mut(idx) {
            if t.sleep_deadline != 0 && t.sleep_deadline <= now {
                t.sleep_deadline = 0;
                t.state = TaskState::Ready;
                woke = true;
                log::trace!("sched: woke sleeper {:?}", t.id);
            }
        }
    }
    if woke {
        poke_idle_secondaries();
    }
    woke
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
/// Phase 8: every tick first wakes expired `SYS_SLEEP` sleepers.  If the
/// tick landed in the core's idle slot (boot context on CPU 0 — kmain
/// resumed because every task was blocked — or a secondary's WFI loop) and
/// a sleeper expired, we switch straight into it; otherwise the normal
/// preemption/syscall-tail points pick the woken task up.
///
/// Phase 11: takes `SCHED_LOCK` for the whole entry.  A real switch
/// releases it mid-`context_switch` (`context_switch_unlock`); when no
/// switch happens the lock is released here.
///
/// # Safety
/// Called from `irq_handler` (interrupts masked by hardware).
pub unsafe fn tick_preempt(from_el0: bool) {
    let lock = sched_lock();
    lock.lock();
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let cpu = crate::smp::cpu_index();
    let cur = crate::smp::current_idx();
    let idle = crate::smp::idle_slot(cpu);
    let woke = wake_sleepers();

    // Reap a task that was killed while running (e.g. parked in the
    // SYS_WAIT_IRQ wait loop): the killer only marks it Zombie — the
    // victim's own core must switch away from it at its next scheduling
    // point, and the tick is that point even inside the kernel.
    let cur_zombie = sched.tasks[cur]
        .as_ref()
        .map(|t| t.state == TaskState::Zombie)
        .unwrap_or(false);
    if cur_zombie {
        let next = sched.pick_next(cpu);
        if next != cur && next != idle {
            let from = sched.ctx_ptr(cur);
            let to = sched.ctx_ptr(next);
            sched.set_state(next, TaskState::Running);
            crate::smp::set_current(next);
            context_switch_unlock(lock, from, to);
            return; // lock released mid-switch
        }
        lock.unlock();
        return;
    }

    if cur == idle {
        // Idle context: normally untouchable, but an expired sleeper needs
        // the CPU handed over (nothing else can run the scheduler).
        if woke {
            let next = sched.pick_next(cpu);
            if next != idle {
                let from = sched.ctx_ptr(cur);
                let to = sched.ctx_ptr(next);
                sched.set_state(next, TaskState::Running);
                crate::smp::set_current(next);
                context_switch_unlock(lock, from, to);
                return; // lock released mid-switch
            }
        }
        lock.unlock();
        return;
    }
    if !from_el0 {
        lock.unlock();
        return; // tick landed in the kernel — syscall tails pick up sleepers
    }
    if !switch_best(false) {
        lock.unlock();
    }
}

/// Cooperative `SYS_YIELD` — hand the CPU to a strictly higher-priority
/// task, if any is runnable (equal-priority rotation happens on ticks).
///
/// # Safety / locking
/// Called from the syscall dispatcher (EL1, interrupts masked) *without*
/// `SCHED_LOCK` held (the dispatcher drops it around `SYS_YIELD`-style
/// helpers that may switch).  Acquires and releases the lock itself; a
/// real switch releases it mid-`context_switch`, so on return the lock is
/// never held.
pub unsafe fn yield_cpu() {
    let lock = sched_lock();
    lock.lock();
    if !switch_best(true) {
        lock.unlock();
    }
}

/// Preemption check run at the end of every syscall: if the syscall woke a
/// task that is strictly higher priority than the caller, switch to it
/// immediately (wakeup-driven preemption — the message gets delivered
/// without waiting for the next tick).  A merely-ready higher-priority task
/// does not steal the CPU: it gets its turn via the tick rotation, so a
/// busy compositor cannot starve the apps.
///
/// Phase 11: the syscall dispatcher runs with `SCHED_LOCK` held, so this
/// is the lock-held variant (never locks itself).  A real switch releases
/// the lock mid-`context_switch`; when no switch happens the caller
/// unlocks.  Returns `true` if a switch happened (the caller's lock is
/// then already free).
///
/// # Safety
/// Called from the syscall dispatcher (EL1, interrupts masked, `SCHED_LOCK`
/// held).
pub unsafe fn reschedule() -> bool {
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let cpu = crate::smp::cpu_index();
    let Some(wslot) = sched.take_woken() else {
        return false;
    };
    let cur = crate::smp::current_idx();
    if cur == crate::smp::idle_slot(cpu) {
        return false; // idle context — never preempt it here (its loop handles picks)
    }
    let cur_prio = sched.priority_and_name(cur).map(|(p, _)| p).unwrap_or(255);
    let w_prio = sched
        .priority_and_name(wslot)
        .map(|(p, _)| p)
        .unwrap_or(255);
    if w_prio >= cur_prio {
        return false; // woken task is not strictly better — no preemption
    }
    log::trace!(
        "sched: wake-preempt '{}'(p{}) -> '{}'(p{})",
        sched.priority_and_name(cur).map(|(_, n)| n).unwrap_or("?"),
        cur_prio,
        sched.priority_and_name(wslot).map(|(_, n)| n).unwrap_or("?"),
        w_prio,
    );
    let from = sched.ctx_ptr(cur);
    let to = sched.ctx_ptr(wslot);
    sched.set_state(cur, TaskState::Ready);
    sched.set_state(wslot, TaskState::Running);
    crate::smp::set_current(wslot);
    context_switch_unlock(sched_lock(), from, to);
    true
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
///
/// Phase 11: acquires `SCHED_LOCK`.  The victim's own core switches away
/// from it at its next scheduling point (the tick preempts EL0 tasks every
/// ms; the syscall tail catches the kernel side), so a zombie that is
/// Running when killed is reaped within one tick — no cross-core signal
/// needed.  A zombie that is Blocked is never woken: every wake site skips
/// non-Ready/Zombie tasks.
pub fn kill_task(id: TaskId) -> bool {
    let lock = sched_lock();
    lock.lock();
    let r = unsafe { kill_task_locked(id) };
    lock.unlock();
    r
}

/// `kill_task` with `SCHED_LOCK` already held (callers: the syscall
/// dispatcher, which runs the whole syscall under the lock).
pub unsafe fn kill_task_locked(id: TaskId) -> bool {
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

/// Terminate the calling task: mark it zombie and switch away.
///
/// Phase 11: takes `SCHED_LOCK`; the switch releases it mid-`context_switch`.
/// The zombie is never re-picked, so this never returns.
pub fn exit_current() -> ! {
    let lock = sched_lock();
    lock.lock();
    unsafe {
        let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
        let cpu = crate::smp::cpu_index();
        let idx = crate::smp::current_idx();
        sched.set_state(idx, TaskState::Zombie);
        log::debug!(
            "task {:?} '{}' exited",
            sched.current_id(),
            sched.current_name()
        );
        // Switch to the next runnable task; nothing must return here.
        let next = sched.pick_next(cpu);
        let from = sched.ctx_ptr(idx);
        let to = sched.ctx_ptr(next);
        sched.set_state(next, TaskState::Running);
        crate::smp::set_current(next);
        context_switch_unlock(lock, from, to);
    }
    unreachable!("exit_current resumed a zombie")
}

/// Enter the scheduler from the kernel boot context (called from `kmain`).
///
/// The current (kernel) context is parked in the idle slot (task 0); the
/// scheduler then runs server tasks.  When every task is blocked or zombie,
/// round-robin falls back to the idle slot and this function returns,
/// resuming `kmain` after all servers have finished.
///
/// # Safety / locking (Phase 11)
/// Must be called exactly once, after at least one server task was spawned
/// and after `smp::bring_up()`.  Takes `SCHED_LOCK`; the switch releases
/// it mid-`context_switch`.
pub unsafe fn enter() {
    let lock = sched_lock();
    lock.lock();
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let cpu = crate::smp::cpu_index();
    let idle = crate::smp::idle_slot(cpu);

    let next = sched.pick_next(cpu);
    if next == idle {
        lock.unlock();
        return; // nothing spawned — nothing to run
    }

    let from = sched.ctx_ptr(idle); // idle slot = our boot context
    let to = sched.ctx_ptr(next);
    sched.set_state(next, TaskState::Running);
    crate::smp::set_current(next);

    context_switch_unlock(lock, from, to);

    // Resumed only when every server task blocked/zombie → kmain continues.
    // The lock was released mid-switch above — it is *not* held here.
    log::debug!("scheduler: back in kernel boot context");
}

/// Perform a round-robin context switch.
///
/// Phase 11: takes `SCHED_LOCK`; a real switch releases it
/// mid-`context_switch`.
///
/// # Safety
/// Must be called with interrupts disabled.
pub unsafe fn schedule() {
    let lock = sched_lock();
    lock.lock();
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    let cpu = crate::smp::cpu_index();

    let current_idx = crate::smp::current_idx();
    let next_idx = sched.pick_next(cpu);

    if current_idx == next_idx {
        lock.unlock();
        return;
    }

    let from = sched.ctx_ptr(current_idx);
    let to = sched.ctx_ptr(next_idx);

    if from.is_null() || to.is_null() {
        lock.unlock();
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
    crate::smp::set_current(next_idx);

    context_switch_unlock(lock, from, to);
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
    ///
    /// Assumes `SCHED_LOCK` is already held; releases it **between** saving
    /// the current context and restoring the next one (Phase 11 — see the
    /// module header for the full locking contract).
    pub(crate) fn context_switch(from: *mut Context, to: *const Context);
    pub(crate) fn context_switch_unlock(
        lock: *const SpinLock,
        from: *mut Context,
        to: *const Context,
    );
}

/// SGI-poke every online secondary that is parked idle in WFI, so it
/// re-runs `pick_next` (Phase 11).  Call with `SCHED_LOCK` held.
///
/// A secondary is "idle" when its idle slot task is Running (the core
/// parked in `secondary_enter`).  The SGI (INTID 3) wakes it from `wfi` —
/// a parked core would otherwise sleep forever even though a task became
/// runnable; its idle loop then re-locks and re-picks.
pub unsafe fn poke_idle_secondaries() {
    let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
    for cpu in 1..crate::smp::MAX_CPUS {
        if !crate::smp::is_online(cpu) {
            continue;
        }
        let idle = crate::smp::idle_slot(cpu);
        let idle_running = sched.tasks[idle]
            .as_ref()
            .map(|t| t.state == TaskState::Running)
            .unwrap_or(false);
        if idle_running {
            log::trace!("sched: poking idle CPU {} (SGI 3)", cpu);
            crate::arch::aarch64::gic::send_sgi(cpu, 3);
        }
    }
}

/// SGI-poke every *other* online CPU (Phase 11).  Called by the device-IRQ
/// handler after recording the pending bit: a task in the `SYS_WAIT_IRQ`
/// wait loop sleeps in `wfi` on whatever core it happens to run — the bit
/// is set here, on the IRQ's core, so sleepers elsewhere must be woken to
/// re-check it (a `wfi` on another core never sees the bit otherwise).
/// Also re-wakes parked idle secondaries.
///
/// A `dsb ish` orders the pending-bit store before the SGIs, so a woken
/// core never re-checks before the bit is visible.
pub unsafe fn poke_other_cpus() {
    core::arch::asm!("dsb ish", options(nomem, nostack));
    let me = crate::smp::cpu_index();
    for cpu in 1..crate::smp::MAX_CPUS {
        if cpu != me && crate::smp::is_online(cpu) {
            log::trace!("smp: poking CPU {} after device IRQ", cpu);
            crate::arch::aarch64::gic::send_sgi(cpu, 3);
        }
    }
}

/// Secondary-core entry (Phase 11): park in this CPU's idle slot and run
/// the global runqueue.
///
/// The idle slot's context was left zeroed by `add_idle` — the first
/// `context_switch_unlock` *saves* this boot context into it, exactly like
/// slot 0 captures the kmain boot context.  When the idle task is picked
/// again it is set Ready; the slot's Running state is what marks this CPU
/// as "parked" for `poke_idle_secondaries`.
///
/// The loop runs with IRQs unmasked: the tick PPI keeps ticking (waking
/// sleepers) and the SGI 3 run-queue poke wakes us when a task becomes
/// runnable on another core.
pub unsafe fn secondary_enter(cpu: usize) -> ! {
    let idle = crate::smp::idle_slot(cpu);
    log::info!("smp: CPU {} parked in idle slot {}", cpu, idle);

    loop {
        let lock = sched_lock();
        lock.lock();
        let sched = &mut *core::ptr::addr_of_mut!(SCHEDULER);
        let next = sched.pick_next(cpu);
        if next != idle {
            let from = sched.ctx_ptr(idle);
            let to = sched.ctx_ptr(next);
            sched.set_state(idle, TaskState::Ready);
            sched.set_state(next, TaskState::Running);
            crate::smp::set_current(next);
            context_switch_unlock(lock, from, to);
            // Resumed when a task on this CPU blocked / exited / was
            // ticked: re-check the runqueue from the top.
            continue;
        }
        lock.unlock();
        // Park with IRQs unmasked so the tick and SGI can wake us.
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
        core::arch::asm!("wfi", options(nomem, nostack));
        core::arch::asm!("msr daifset, #2", options(nomem, nostack));
    }
}
