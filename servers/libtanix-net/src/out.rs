//! Outbound frame queue.
//!
//! The stack builds its replies and app-initiated packets into a fixed
//! scratch buffer with a small list of frame boundaries.  The caller (the
//! net server) flushes the queue through the one-TX-slot NIC driver after
//! every `on_frame` / `poll` / send call.  Losing a frame here is a
//! non-event: TCP retransmits cover SYNs/SYN-ACKs/data, and duplicate ACKs
//! are harmless, so the queue is allowed to overflow rather than grow.

/// Maximum eth+IPv4+TCP segment that we ever build (1518 minus nothing —
/// our MSS cap keeps segments well under this).
pub const FRAME_MAX: usize = 1518;

/// Frames per flush.
pub const MAX_FRAMES: usize = 4;

pub struct OutQueue {
    pub buf: [u8; FRAME_MAX],
    /// Cumulative end offsets: frame `i` is `buf[off[i]..off[i + 1]]`.
    off: [u16; MAX_FRAMES + 1],
    n: usize,
}

impl OutQueue {
    pub const fn new() -> Self {
        Self {
            buf: [0; FRAME_MAX],
            off: [0; MAX_FRAMES + 1],
            n: 0,
        }
    }

    /// Reserve `len` bytes for one frame; returns its start offset (or
    /// None when the queue is full or the scratch buffer exhausted).
    pub fn push(&mut self, len: usize) -> Option<usize> {
        if self.n >= MAX_FRAMES {
            return None;
        }
        let start = self.off[self.n] as usize;
        let end = start + len;
        if end > FRAME_MAX {
            return None;
        }
        self.off[self.n + 1] = end as u16;
        self.n += 1;
        Some(start)
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn frame(&self, i: usize) -> &[u8] {
        debug_assert!(i < self.n);
        &self.buf[self.off[i] as usize..self.off[i + 1] as usize]
    }

    pub fn clear(&mut self) {
        self.n = 0;
        self.off[0] = 0;
    }
}

impl Default for OutQueue {
    fn default() -> Self {
        Self::new()
    }
}