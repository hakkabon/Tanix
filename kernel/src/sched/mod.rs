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

/// The kernel's system-call table, passed to every server at boot.
///
/// All calls take/return value arguments (plus pointers into the caller's
/// own memory).  A server calls these through the `SyscallTable` pointer it
/// receives in its boot info; in a future EL0 split the same table becomes
/// the SVC dispatch index.
#[repr(C)]
pub struct SyscallTable {
    /// `send(dst, msg) -> 0 | -errno` — blocking rendezvous with `dst`.
    pub send: unsafe extern "C" fn(u32, *const Message) -> i32,
    /// `receive(filter, out) -> src | -errno` — blocks until a message from
    /// a sender matching `filter` (M_ANY = any) is delivered into `out`.
    pub receive: unsafe extern "C" fn(i32, *mut Message) -> i32,
    /// `spawn(name) -> new task id | -errno` — start a registered server.
    pub spawn: unsafe extern "C" fn(*const u8) -> i32,
    /// `who(name) -> task id | -1` — resolve a server name to its task id.
    pub who: unsafe extern "C" fn(*const u8) -> i32,
    /// `exit_task(pid) -> 0 | -errno` — kill another task.
    pub exit_task: unsafe extern "C" fn(u32) -> i32,
    /// `exit() -> !` — terminate the calling task.
    pub exit: unsafe extern "C" fn() -> !,
    /// `alloc_frames(n) -> phys base | 0` — allocate n contiguous frames.
    pub alloc_frames: unsafe extern "C" fn(u32) -> u64,
    /// `free_frames(base, n) -> 0 | -errno` — release frames.
    pub free_frames: unsafe extern "C" fn(u64, u32) -> i32,
    /// `log(level, msg)` — kernel log line, prefixed with the caller's name.
    pub log: unsafe extern "C" fn(u32, *const u8),
}

/// Boot info block handed to every server task (in its callee-saved x19).
#[repr(C)]
pub struct BootInfo {
    /// Kernel syscall table.
    pub syscalls: *const SyscallTable,
    /// This task's own id.
    pub task_id: u32,
}

/// A sender that is blocked waiting for a receiver to accept its message.
#[derive(Clone, Copy)]
pub struct PendingSend {
    pub src: u32,
    pub buf: *const Message,
}
