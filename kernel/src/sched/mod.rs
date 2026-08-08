#![allow(dead_code)]
//! Scheduler subsystem.
//!
//! Provides:
//!   • `TaskId` / `TaskState` — core scheduler types.
//!   • `task`   — Task control block, Context, and round-robin Scheduler.

pub mod task;

pub use task::enter;

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
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct BootInfo {
    /// This task's own id.
    pub task_id: u32,
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
