#![allow(dead_code)]
//! Kernel-side VirtIO transport: initialise queues, send messages, poll replies.
//!
//! The kernel is the "driver" side of the virtqueue — it posts buffers into
//! the avail ring and reads replies from the used ring.  The guest Zephyr
//! stub is the "device" side — it processes avail entries and writes to used.
//!
//! Doorbell mechanism:
//!   After posting to the avail ring the kernel fires a GIC SGI to wake the
//!   guest.  The guest replies by writing to the used ring and firing its own
//!   SGI back.

use super::{
    VirtqueueConfig, Virtqueue, BUF_SIZE, OFF_CONFIG, VIRTQ_MAGIC,
    QUEUE_SIZE,
};
use super::channel::{Msg, Opcode};
use crate::hypervisor::{DoorbellHandle, Hypervisor};
use crate::mem::PhysAddr;

/// Kernel-side transport state.
pub struct VirtioTransport {
    /// Physical base of the shared-memory region.
    pub shmem_phys: PhysAddr,
    /// Live virtqueue accessor.
    pub vq: Virtqueue,
    /// Next descriptor index to use for TX (cycles 0..QUEUE_SIZE).
    tx_desc: u16,
    /// Doorbell handle used to notify the guest.
    doorbell: DoorbellHandle,
}

impl VirtioTransport {
    /// Initialise the virtqueue in `shmem_phys` and return a transport handle.
    ///
    /// This writes the `VirtqueueConfig` header into the shared region so the
    /// guest can find the ring addresses.
    ///
    /// # Safety
    /// `shmem_phys` must be a valid, identity-mapped physical address pointing
    /// to at least `OFF_BUFFERS + QUEUE_SIZE * BUF_SIZE` bytes of zeroed memory.
    pub unsafe fn new(shmem_phys: PhysAddr, doorbell: DoorbellHandle) -> Self {
        let base = shmem_phys as *mut u8;

        // Write the configuration block.
        let cfg = &mut *(base.add(OFF_CONFIG) as *mut VirtqueueConfig);
        cfg.queue_size = QUEUE_SIZE as u32;
        cfg.desc_phys  = (shmem_phys + super::OFF_DESC)    as u64;
        cfg.avail_phys = (shmem_phys + super::OFF_AVAIL)   as u64;
        cfg.used_phys  = (shmem_phys + super::OFF_USED)    as u64;
        cfg.buf_phys   = (shmem_phys + super::OFF_BUFFERS) as u64;
        cfg.buf_size   = BUF_SIZE as u32;
        cfg.tx_slots   = QUEUE_SIZE as u32;
        cfg._pad       = [0u8; 16];

        // Memory barrier before writing magic — guest checks magic last.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        core::ptr::write_volatile(&mut cfg.magic, VIRTQ_MAGIC);

        log::info!(
            "virtio: queue initialised at {:#x} (desc={:#x} avail={:#x} used={:#x})",
            shmem_phys,
            cfg.desc_phys, cfg.avail_phys, cfg.used_phys
        );

        VirtioTransport {
            shmem_phys,
            vq: Virtqueue::from_phys(shmem_phys),
            tx_desc: 0,
            doorbell,
        }
    }

    /// Send a `Print` message to the guest.
    ///
    /// Returns the descriptor index used, so `poll_replies` can match it.
    pub unsafe fn send_print(
        &mut self,
        text: &[u8],
        hv: &mut dyn Hypervisor,
    ) -> u16 {
        let desc_idx = self.tx_desc;
        self.tx_desc = (self.tx_desc + 1) % QUEUE_SIZE as u16;

        let buf = self.vq.buf_ptr(desc_idx as usize);
        let len = Msg::write_print(buf, text);

        self.vq.post_avail(desc_idx, len);

        // Notify guest via doorbell.
        if let Err(e) = hv.doorbell_send(self.doorbell) {
            log::warn!("virtio: doorbell_send failed: {:?}", e);
        }

        log::debug!(
            "virtio: PRINT sent desc={} len={} text='{}'",
            desc_idx, len,
            core::str::from_utf8(&text[..text.len().min(32)]).unwrap_or("?")
        );

        desc_idx
    }

    /// Poll the used ring for guest replies.
    ///
    /// Calls `on_reply(desc_idx, opcode, printed_bytes)` for each completed
    /// buffer.
    pub unsafe fn poll_replies<F>(&mut self, mut on_reply: F)
    where
        F: FnMut(u16, Opcode, u32),
    {
        // Extract the raw buffer base pointer before entering the closure so
        // we don't hold a second borrow of `self.vq` while `poll_used` runs.
        let bufs_base = self.vq.bufs;
        self.vq.poll_used(|desc_idx, _written| {
            let buf = bufs_base.add(desc_idx as usize * BUF_SIZE);
            let msg = Msg::from_ptr(buf);
            if let Some(op) = msg.opcode() {
                let extra = if op == Opcode::Echo {
                    Msg::read_echo(buf)
                } else {
                    0
                };
                on_reply(desc_idx, op, extra);
            }
        });
    }

    /// Busy-wait until at least one reply arrives (or `timeout_ms` elapses).
    ///
    /// Returns `true` if a reply arrived within the timeout.
    pub unsafe fn wait_reply(
        &mut self,
        timeout_ms: u64,
        hv: &mut dyn Hypervisor,
    ) -> bool {
        use crate::arch::aarch64::timer;
        let freq   = timer::frequency();
        let ticks  = freq / 1000 * timeout_ms;
        let start  = timer::read_count();
        let mut got_reply = false;

        loop {
            self.poll_replies(|desc_idx, op, printed| {
                log::info!(
                    "virtio: reply desc={} op={:?} printed={}",
                    desc_idx, op, printed
                );
                got_reply = true;
            });

            if got_reply {
                return true;
            }

            if timer::read_count().wrapping_sub(start) >= ticks {
                return false;
            }

            core::hint::spin_loop();
            let _ = hv; // suppress unused-variable warning
        }
    }
}
