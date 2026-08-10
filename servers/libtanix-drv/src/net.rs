//! virtio-net driver — Phase 10.
//!
//! Owns the two vrings (RX = device→driver, TX = driver→device) and the
//! buffer pools:
//!
//!   • RX pool: `RX_COUNT` (32) fixed slots × `RX_BUF_SIZE` (2048) B in
//!     one contiguous 64 KiB allocation.  Slots 0..31 are permanently
//!     armed: every slot drained from the used ring is immediately
//!     re-published with the same buffer.
//!   • TX: a single static slot (`TX_SLOT = 32`) with one 4 KiB frame for
//!     the packet plus its 12-byte virtio-net header.  At most one
//!     transmit is in flight (`tx_done` flag), which is all a
//!     single-threaded event loop needs.
//!
//! QEMU's user-mode (slirp) netdev cannot do vnet headers, so the driver
//! works with the fallback layout: on TX the guest prepends a 12-byte
//! (zeroed) virtio_net_hdr that the device strips; on RX the device
//! writes a zeroed 12-byte header in front of the ethernet frame, which
//! the driver skips (`NET_HDR_SIZE`).

use core::ptr;

use tanix_libsys::sys;

use crate::virtio_pci::VirtioPci;
use crate::vring::{Vring, DESC_WRITE};

/// virtio-net device id.
pub const VIRTIO_NET_S: u16 = 1;

// Feature bits we negotiate (QEMU supports both unconditionally).
const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
// VIRTIO_F_VERSION_1: with it, the device uses the 12-byte mrg_rxbuf-style
// vnet header on RX/TX (our NET_HDR_SIZE); without it, QEMU falls back to
// the 10-byte legacy header and every frame is off by two bytes.
const F_VERSION_1: u64 = 1 << 32;

// Device-config offsets.
const CFG_MAC: usize = 0;
const CFG_STATUS: usize = 6;

// Queues.
const QUEUE_RX: u16 = 0;
const QUEUE_TX: u16 = 1;

// Buffer pools.
const RX_COUNT: u16 = 32;
const RX_BUF_SIZE: usize = 2048;
const TX_SLOT: u16 = 32;
const TX_BUF_SIZE: usize = 2048;
const NET_HDR_SIZE: usize = 12;

/// An open virtio-net device.
pub struct VirtioNet {
    pub dev: VirtioPci,
    /// MAC address as configured by the device.
    pub mac: [u8; 6],
    /// Link status from the device config (bit 0 = link up).
    pub status: u16,
    rx: Vring,
    tx: Vring,
    rx_base: u64,
    tx_base: u64,
    tx_done: bool,
}

impl VirtioNet {
    /// Probe and bring up the virtio-net device.
    pub fn open() -> Option<VirtioNet> {
        let dev = VirtioPci::open(VIRTIO_NET_S)?;

        dev.reset();
        let feats = dev.negotiate(F_VERSION_1 | F_MAC | F_STATUS)?;
        if feats & F_VERSION_1 == 0 {
            dev.fail("virtio-net: device has no VERSION_1 feature");
            return None;
        }
        if feats & F_MAC == 0 {
            dev.fail("virtio-net: device has no MAC feature");
            return None;
        }

        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = dev.cfg_read8(CFG_MAC + i);
        }
        let status = dev.cfg_read8(CFG_STATUS) as u16;

        // Buffer pools (identity mapped, directly dereferenceable).
        let rx_base = sys::alloc_frames(RX_COUNT as u32 * RX_BUF_SIZE as u32 / 4096);
        let tx_base = sys::alloc_frames(TX_BUF_SIZE.div_ceil(4096) as u32);
        if rx_base == 0 || tx_base == 0 {
            sys::log(1, "virtio-net: buffer alloc failed");
            return None;
        }
        let rx = Vring::new()?;
        let tx = Vring::new()?;

        if dev.setup_queue(QUEUE_RX, &rx) == 0 || dev.setup_queue(QUEUE_TX, &tx) == 0 {
            return None;
        }

        let mut net = VirtioNet {
            dev,
            mac,
            status,
            rx,
            tx,
            rx_base,
            tx_base,
            tx_done: true,
        };

        // Arm every RX slot and hand the whole pool to the device.
        for i in 0..RX_COUNT {
            net.rx.write_desc(i, net.rx_base + i as u64 * RX_BUF_SIZE as u64, RX_BUF_SIZE as u32, DESC_WRITE, 0);
            net.rx.publish(i);
        }

        // The device only starts processing the avail ring once it is in
        // DRIVER_OK; kicks sent earlier (the arm loop above) are dropped,
        // so notify again after `driver_ok()`.
        dev.driver_ok();
        net.dev.notify(QUEUE_RX);
        Some(net)
    }

    /// True if the device has returned any used entries (RX ring).
    pub fn rx_pending(&self) -> bool {
        self.rx.has_used()
    }

    /// True if the last transmit has completed.
    pub fn tx_idle(&self) -> bool {
        self.tx_done
    }

    /// Handle all used entries currently returned on the TX ring.
    /// (RX slots are re-armed inside `recv`.)
    pub fn poll_tx(&mut self) {
        let mut used = [(0u16, 0u32); 8];
        let n = self.tx.drain_used(&mut used);
        for (id, _) in used[..n].iter() {
            if *id == TX_SLOT {
                self.tx_done = true;
            }
        }
    }

    /// Receive one ethernet frame (payload after the 12-byte header) into
    /// `buf`; returns its length, re-arming the consumed RX slot.  Returns
    /// None when no complete frame is pending.
    pub fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let mut used = [(0u16, 0u32); 8];
        let n = self.rx.drain_used(&mut used);
        for (id, len) in used[..n].iter() {
            // Re-arm the slot regardless of how we consume it below.
            self.rx.write_desc(*id, self.rx_base + *id as u64 * RX_BUF_SIZE as u64, RX_BUF_SIZE as u32, DESC_WRITE, 0);
            self.rx.publish(*id);
            self.dev.notify(QUEUE_RX);

            let payload = *len as usize;
            if payload <= NET_HDR_SIZE || payload - NET_HDR_SIZE > buf.len() {
                continue;
            }
            let src = (self.rx_base + *id as u64 * RX_BUF_SIZE as u64) as *const u8;
            let n = payload - NET_HDR_SIZE;
            unsafe { ptr::copy_nonoverlapping(src.add(NET_HDR_SIZE), buf.as_mut_ptr(), n) };
            return Some(n);
        }
        None
    }

    /// Transmit one ethernet frame.  Fails if a TX is still in flight.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !self.tx_done || frame.len() > TX_BUF_SIZE - NET_HDR_SIZE {
            return false;
        }
        let buf = self.tx_base as *mut u8;
        unsafe {
            ptr::write_bytes(buf, 0, NET_HDR_SIZE);
            ptr::copy_nonoverlapping(frame.as_ptr(), buf.add(NET_HDR_SIZE), frame.len());
        }
        let total = NET_HDR_SIZE + frame.len();
        self.tx.write_desc(TX_SLOT, self.tx_base, total as u32, 0, 0);
        self.tx.publish(TX_SLOT);
        self.dev.notify(QUEUE_TX);
        self.tx_done = false;
        true
    }
}
