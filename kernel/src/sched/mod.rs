#![allow(dead_code)]
//! Scheduler subsystem.
//!
//! Provides:
//!   • `TaskId` / `TaskState` — core scheduler types.
//!   • `task`   — Task control block, Context, and round-robin Scheduler.

pub mod task;

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
