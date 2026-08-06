#![allow(dead_code)]
//! Typed message protocol over the VirtIO transport.
//!
//! Phase 3 defines a minimal two-message protocol:
//!
//!   `Print`  — kernel → guest: "please print this string to UART"
//!   `Echo`   — guest  → kernel (reply): "I printed N bytes"
//!
//! Every message starts with a 1-byte opcode followed by opcode-specific
//! fields.  Total message size ≤ BUF_SIZE (256 bytes).
//!
//! Layout:
//!   [0]      opcode   (u8)
//!   [1..3]   reserved (2 bytes, zero)
//!   [3]      length   (u8)  — number of payload bytes that follow
//!   [4..]    payload  (up to 252 bytes)

use super::BUF_SIZE;

/// Message opcodes.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    /// Kernel → guest: print payload string.
    Print = 0x01,
    /// Guest → kernel: echo / acknowledgement, payload = bytes printed (u32 LE).
    Echo  = 0x02,
}

/// Maximum payload bytes in a single message.
pub const MAX_PAYLOAD: usize = BUF_SIZE - 4;

/// An in-place message view over a raw buffer.
///
/// The buffer is owned by the VirtQueue (in shared memory); we never copy it.
pub struct Msg<'a> {
    buf: &'a mut [u8; BUF_SIZE],
}

impl<'a> Msg<'a> {
    /// Wrap a mutable buffer slice as a message.
    ///
    /// # Safety
    /// `ptr` must point to a `BUF_SIZE`-byte region in the shared-memory
    /// VirtQueue.
    pub unsafe fn from_ptr(ptr: *mut u8) -> Self {
        Self { buf: &mut *(ptr as *mut [u8; BUF_SIZE]) }
    }

    pub fn opcode(&self) -> Option<Opcode> {
        match self.buf[0] {
            0x01 => Some(Opcode::Print),
            0x02 => Some(Opcode::Echo),
            _    => None,
        }
    }

    pub fn payload_len(&self) -> usize {
        self.buf[3] as usize
    }

    pub fn payload(&self) -> &[u8] {
        let len = self.payload_len().min(MAX_PAYLOAD);
        &self.buf[4..4 + len]
    }

    // ── Constructors ─────────────────────────────────────────────────────────

    /// Encode a `Print` message into the buffer.
    /// Returns the total wire length (opcode + reserved + len + payload).
    pub fn write_print(buf: *mut u8, text: &[u8]) -> u32 {
        let payload = text.len().min(MAX_PAYLOAD);
        unsafe {
            let b = &mut *(buf as *mut [u8; BUF_SIZE]);
            b[0] = Opcode::Print as u8;
            b[1] = 0;
            b[2] = 0;
            b[3] = payload as u8;
            b[4..4 + payload].copy_from_slice(&text[..payload]);
        }
        (4 + payload) as u32
    }

    /// Encode an `Echo` reply into the buffer.
    /// `printed` = number of bytes the guest printed.
    pub fn write_echo(buf: *mut u8, printed: u32) -> u32 {
        unsafe {
            let b = &mut *(buf as *mut [u8; BUF_SIZE]);
            b[0] = Opcode::Echo as u8;
            b[1] = 0;
            b[2] = 0;
            b[3] = 4; // payload is a u32
            b[4..8].copy_from_slice(&printed.to_le_bytes());
        }
        8u32
    }

    /// Read an `Echo` reply's printed-byte count from the buffer.
    pub fn read_echo(buf: *const u8) -> u32 {
        unsafe {
            let b = &*(buf as *const [u8; BUF_SIZE]);
            u32::from_le_bytes([b[4], b[5], b[6], b[7]])
        }
    }
}
