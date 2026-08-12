#![allow(dead_code)]
//! Scheduler subsystem.
//!
//! Provides:
//!   • `TaskId` / `TaskState` — core scheduler types.
//!   • `task`   — Task control block, Context, and priority Scheduler.
//!
//! Phase 7: the scheduler is preemptive — a periodic timer tick (1 ms)
//! preempts EL0 tasks (highest priority first, round-robin within a
//! priority), and every syscall return re-evaluates the run queue so a
//! woken higher-priority task runs immediately.
//!
//! Phase 11 (SMP): there is ONE global runqueue guarded by `SCHED_LOCK`.
//! Every scheduling entry point acquires the lock, mutates task states,
//! picks the next task and calls `context_switch_unlock`, whose assembly
//! releases the lock *between* saving the current context and restoring
//! the next one.  Tasks migrate freely between cores; the lock guarantees
//! at most one core executes a given task.  Secondary cores park in their
//! own idle slots (`sched::task::secondary_enter`) and are woken from WFI
//! with an SGI when the runqueue changes.

pub mod task;

pub use task::enter;
pub use task::secondary_enter;

use core::arch::global_asm;

// The context switch stub is in assembly so it can precisely control which
// registers are saved/restored without interference from the compiler.
global_asm!(include_str!("switch.s"));

/// Unique task identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskId(pub u32);

/// Task states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Ready,
    Running,
    Blocked,
    Zombie,
}

// ── Priorities (Phase 7) ──────────────────────────────────────────────────────
//
// Lower number = higher priority.  Idle is the lowest; everything else is
// assigned per server in `server.rs` (display: 32, ui-demo: 96, the
// Phase-4 servers: 128, the `hog` spin demo: 192).

/// The idle slot — never wins the run queue while anything else runs.
pub const PRIO_IDLE: u8 = 255;
/// Default for plain tasks (kernel-side tasks, the Phase-4 servers).
pub const PRIO_NORMAL: u8 = 128;

// ── IPC primitives (shared ABI with the server binaries) ─────────────────────

/// A fixed-size IPC message.  Payload is 32 bytes (8 × u32); long strings
/// are sent inline (up to 28 chars + NUL), exactly like classic Minix.
///
/// Layout must match `servers/libtanix-sys`'s `Message`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Message {
    /// Stamped by the kernel — sender's task id.
    pub src: u32,
    /// Message type (per-server protocol constant).
    pub mtype: u32,
    /// Payload words, interpreted per `mtype`.
    pub data: [u32; 8],
}

impl Message {
    pub const fn new(mtype: u32) -> Self {
        Self { src: 0, mtype, data: [0u32; 8] }
    }
}

/// Sentinel filter for `receive`: accept a message from any sender.
pub const M_ANY: i32 = -1;

/// Boot info block handed to every server task (in its callee-saved x19).
///
/// Phase 6: servers run at EL0 and call the kernel via `svc #0` — there is
/// no function-pointer table anymore, only the task id (and the address of
/// this block, which the task can read through x19).
///
/// Phase 16: `machine` tells servers which machine the kernel booted on
/// (see `arch::aarch64::machine`); drivers pick MMIO windows / IRQs from it.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct BootInfo {
    /// This task's own id.
    pub task_id: u32,
    /// Machine id (0 = QEMU virt, 1 = QEMU sbsa-ref).
    pub machine: u32,
}

/// A sender that is blocked waiting for a receiver to accept its message.
///
/// The message is copied by value at park time (while the sender's address
/// space is still active): since Phase 6 each task has its own TTBR0, a
/// pointer into the sender's memory would not be readable once the
/// scheduler has switched to the receiver's table.
#[derive(Clone, Copy)]
pub struct PendingSend {
    pub src: u32,
    pub msg: Message,
}

/// A message staged on the *sender* because the receiver's
/// `pending_senders` queue was full.  Never dropped: the receiver's
/// `receive` either wakes the sender when a slot frees or delivers the
/// staged message directly when the filter matches (`sys_receive`).
#[derive(Clone, Copy)]
pub struct StagedSend {
    /// Receiver task id the message is destined for.
    pub dst: u32,
    /// Sender task id (message `src`).
    pub src: u32,
    pub msg: Message,
}
