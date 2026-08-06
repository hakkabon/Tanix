#![allow(dead_code)]
//! Synchronous copy-based IPC channels — Minix `send` / `recv` model.
//!
//! Design
//! ──────
//! • A `Channel` is a one-directional, rendezvous pipe: the sender blocks
//!   until the receiver calls `recv`, and vice-versa.
//! • No ring buffers, no async queues — the message is copied directly from
//!   the sender's stack into the receiver's buffer on rendezvous.
//! • In Phase 1 / 2 everything runs on a single core with cooperative
//!   scheduling, so "blocking" means yielding to the scheduler loop.
//!
//! Phase 4 will extend this with capability-based endpoint addressing and
//! a proper wait queue per endpoint.

use super::{EndpointId, Message, MSG_MAX_BYTES};

// ── Channel state machine ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// No pending operation.
    Idle,
    /// A sender has deposited a message and is waiting for the receiver.
    SendPending,
    /// A receiver is waiting for a sender.
    RecvWaiting,
}

/// A single rendezvous channel between two endpoints.
///
/// The channel is NOT thread-safe (no mutex) — Phase 1/2 is single-core.
pub struct Channel {
    pub id: EndpointId,
    pub state: ChannelState,
    /// Staging buffer — filled by `send`, consumed by `recv`.
    pub msg: Message,
}

impl Channel {
    pub const fn new(id: EndpointId) -> Self {
        Self {
            id,
            state: ChannelState::Idle,
            msg: Message {
                sender: EndpointId(0),
                opcode: 0,
                data: [0u8; MSG_MAX_BYTES],
            },
        }
    }

    /// Attempt to deposit `msg` into the channel.
    ///
    /// Returns `Ok(())` if a receiver was already waiting (rendezvous
    /// complete) or `Err(())` if the channel was busy.
    ///
    /// In Phase 2 the scheduler calls `try_send` in a loop (cooperative
    /// spin) until it succeeds; a proper blocking mechanism comes in Phase 4.
    pub fn try_send(&mut self, msg: Message) -> Result<(), ()> {
        match self.state {
            ChannelState::Idle => {
                self.msg = msg;
                self.state = ChannelState::SendPending;
                Ok(())
            }
            ChannelState::RecvWaiting => {
                self.msg = msg;
                self.state = ChannelState::Idle;
                Ok(())
            }
            ChannelState::SendPending => Err(()), // another sender is waiting
        }
    }

    /// Attempt to consume a pending message from the channel.
    ///
    /// Returns `Some(Message)` if one was available, `None` otherwise.
    pub fn try_recv(&mut self, receiver: EndpointId) -> Option<Message> {
        match self.state {
            ChannelState::SendPending => {
                let msg = self.msg;
                self.state = ChannelState::Idle;
                Some(msg)
            }
            ChannelState::Idle => {
                // Signal that we are waiting.
                self.msg.sender = receiver;
                self.state = ChannelState::RecvWaiting;
                None
            }
            ChannelState::RecvWaiting => None, // still waiting
        }
    }
}

// ── Channel table ─────────────────────────────────────────────────────────────

/// Maximum number of IPC channels in Phase 1/2.
pub const MAX_CHANNELS: usize = 16;

pub struct ChannelTable {
    channels: [Option<Channel>; MAX_CHANNELS],
    next_id: u32,
}

impl ChannelTable {
    pub const fn new() -> Self {
        // `Option<Channel>` is not `Copy` so we can't use array init syntax;
        // use a manual unrolled approach with a const.
        Self {
            channels: [
                None, None, None, None, None, None, None, None,
                None, None, None, None, None, None, None, None,
            ],
            next_id: 1,
        }
    }

    /// Create a new channel and return its `EndpointId`.
    pub fn create(&mut self) -> Option<EndpointId> {
        let slot = self.channels.iter_mut().find(|s| s.is_none())?;
        let id = EndpointId(self.next_id);
        self.next_id += 1;
        *slot = Some(Channel::new(id));
        Some(id)
    }

    pub fn get_mut(&mut self, id: EndpointId) -> Option<&mut Channel> {
        self.channels
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|c| c.id == id)
    }
}

/// Global channel table.
static mut CHANNEL_TABLE: ChannelTable = ChannelTable::new();

/// Create a new IPC channel.  Returns the endpoint ID.
pub fn channel_create() -> Option<EndpointId> {
    unsafe { (*core::ptr::addr_of_mut!(CHANNEL_TABLE)).create() }
}

/// Non-blocking send.  Returns `Ok` when the message is accepted.
pub fn channel_send(id: EndpointId, msg: Message) -> Result<(), ()> {
    unsafe {
        (*core::ptr::addr_of_mut!(CHANNEL_TABLE))
            .get_mut(id)
            .ok_or(())?
            .try_send(msg)
    }
}

/// Non-blocking receive.  Returns `Some(msg)` when a message is available.
pub fn channel_recv(id: EndpointId, receiver: EndpointId) -> Option<Message> {
    unsafe {
        (*core::ptr::addr_of_mut!(CHANNEL_TABLE))
            .get_mut(id)?
            .try_recv(receiver)
    }
}
