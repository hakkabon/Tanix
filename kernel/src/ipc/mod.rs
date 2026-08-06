#![allow(dead_code)]
//! Inter-process communication subsystem.
//!
//! Provides:
//!   • `EndpointId` / `Message` — core IPC types.
//!   • `channel`    — synchronous copy-based rendezvous channels.

pub mod channel;

/// Opaque handle identifying a kernel IPC endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointId(pub u32);

/// Maximum payload size for a single synchronous message (bytes).
pub const MSG_MAX_BYTES: usize = 64;

/// A fixed-size, copy-based IPC message.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Message {
    pub sender: EndpointId,
    pub opcode: u32,
    pub data: [u8; MSG_MAX_BYTES],
}
