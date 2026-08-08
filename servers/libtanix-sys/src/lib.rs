//! Tanix server ABI — the contract between the kernel and server binaries.
//!
//! Layouts here must match the kernel's definitions in
//! `kernel/src/sched/mod.rs` (`Message`, `BootInfo`) and
//! `kernel/src/ipc/syscall.rs` (syscall numbers and semantics).

#![no_std]

pub mod abi;
pub mod entry;
pub mod fmt;
pub mod sys;

#[cfg(feature = "alloc")]
pub mod heap;

mod panic;

pub use abi::{BootInfo, Message};
