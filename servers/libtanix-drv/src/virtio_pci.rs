//! Modern (virtio 1.0) virtio-pci transport — Phase 10.
//!
//! The driver discovers the device's four regions through the virtio
//! vendor capabilities in PCI config space (each capability names a BAR,
//! an offset inside it and a size): the **common config** (feature
//! negotiation, device status, queue setup), the **notify** region (one
//! 4-byte slot per queue, at `queue_notify_off × 4`), the **ISR** status
//! register (read-and-clear, deasserts the INTx line) and the **device
//! config** region (virtio-net's MAC/status here).
//!
//! The device is driven in modern mode: 64-bit feature negotiation with
//! the FEATURES_OK step, per-queue enable via the common config (three
//! ring addresses + `queue_enable`), and MSI-X left disabled so the
//! device falls back to its legacy INTx line — QEMU raises it while its
//! ISR status is nonzero and drops it when the driver reads the ISR
//! register.  The kernel records the delivery (`SYS_IRQ_PENDING` /
//! `SYS_WAIT_IRQ`) and the server deasserts the line after draining.
//!
//! MMIO windows (ECAM, BARs) are mapped into the task's address space
//! with `SYS_MAP_DEVICE`.

use core::ptr;

use tanix_libsys::sys;

use crate::pci::{self, Bdf, VirtioCap};
use crate::vring::Vring;

// ── Device status bits ───────────────────────────────────────────────────────

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 128;

// ── Common config register offsets ───────────────────────────────────────────

const C_DEVICE_FEATURE_SEL: usize = 0x00;
const C_DEVICE_FEATURE: usize = 0x04;
const C_DRIVER_FEATURE_SEL: usize = 0x08;
const C_DRIVER_FEATURE: usize = 0x0C;
const C_DEVICE_STATUS: usize = 0x14;
const C_QUEUE_SELECT: usize = 0x16;
const C_QUEUE_SIZE: usize = 0x18;
const C_QUEUE_ENABLE: usize = 0x1C;
const C_QUEUE_DESC: usize = 0x20;
const C_QUEUE_DRIVER: usize = 0x28;
const C_QUEUE_DEVICE: usize = 0x30;

/// A modern virtio-pci device.
#[derive(Clone, Copy)]
pub struct VirtioPci {
    pub bdf: Bdf,
    /// Identity-mapped MMIO bases of the four capability regions.
    pub common: usize,
    pub notify: usize,
    pub isr: usize,
    pub device_cfg: usize,
    /// GIC SPI for the legacy INTx line.
    pub irq: u32,
    /// Bytes per queue slot in the notify region.
    pub notify_mult: u32,
}

impl VirtioPci {
    /// Probe for a virtio device with the given device id and map its
    /// regions.  Returns None if not present or anything fails.
    pub fn open(device_id: u16) -> Option<VirtioPci> {
        if pci::map_ecam().is_err() {
            sys::log(1, "virtio-pci: ECAM map failed");
            return None;
        }
        let bdf = pci::find_virtio(device_id)?;
        let irq = pci::intx_irq(bdf);

        // Collect the virtio capabilities.
        let mut caps = [VirtioCap { cfg_type: 0, bar: 0, offset: 0, size: 0, notify_mult: 0 }; 8];
        let n = pci::virtio_caps(bdf, &mut caps);
        if n == 0 {
            sys::log(1, "virtio-pci: no VNDR caps");
            return None;
        }

        // Map each BAR once (capabilities may share one BAR — QEMU packs
        // common/isr/device/notify into a single 32 KiB BAR).  The map
        // must cover the whole span of the regions on the BAR, not just
        // the first capability's size, so compute the span per BAR first.
        // The PCI_CFG portal cap (type 5) is skipped: it points at the
        // legacy I/O bar slot, which modern-only devices leave unmapped.
        // BARs that came up zero get a guest-assigned address inside the
        // 32-bit PCI window (see `pci::assign_bar` — QEMU's mmio64 window
        // vanishes with `highmem=off`, leaving 64-bit prefetchable BARs
        // unassigned).
        let mut window = 0x3EFE_0000u64;
        let mut mapped_bars = [0u64; 6];
        let mut bar_span = [0usize; 6];
        for cap in &caps[..n] {
            let bar_idx = cap.bar as usize;
            if cap.cfg_type == pci::CAP_CFG_PCI || bar_idx >= 6 {
                continue;
            }
            let end = cap.offset as usize + cap.size as usize;
            if end > bar_span[bar_idx] {
                bar_span[bar_idx] = end;
            }
        }
        for cap in &caps[..n] {
            let bar_idx = cap.bar as usize;
            if cap.cfg_type == pci::CAP_CFG_PCI || bar_idx >= 6 {
                continue;
            }
            if mapped_bars[bar_idx] == 0 {
                let mut base = pci::bar(bdf, bar_idx);
                if base == 0 {
                    window -= bar_span[bar_idx] as u64;
                    pci::assign_bar(bdf, bar_idx, window);
                    base = pci::bar(bdf, bar_idx);
                }
                if base == 0 {
                    sys::log(1, "virtio-pci: empty BAR");
                    return None;
                }
                let size = 4096usize.max(bar_span[bar_idx]);
                if pci::map_mmio(base, size).is_err() {
                    sys::log(1, "virtio-pci: BAR map failed");
                    return None;
                }
                mapped_bars[bar_idx] = base;
            }
        }
        pci::enable_device(bdf);

        // Resolve the four regions from the capabilities.
        let mut common = 0usize;
        let mut notify = 0usize;
        let mut notify_mult = 4u32;
        let mut isr = 0usize;
        let mut device_cfg = 0usize;
        for cap in &caps[..n] {
            let bar_base = mapped_bars[cap.bar as usize];
            if bar_base == 0 {
                continue;
            }
            let region = (bar_base as usize) + cap.offset as usize;
            match cap.cfg_type {
                pci::CAP_CFG_COMMON => common = region,
                pci::CAP_CFG_NOTIFY => {
                    notify = region;
                    // Per-queue stride from the cap's notify_off_multiplier
                    // (QEMU writes its queue_mem_mult: 4, or a page when
                    // the "page per vq" flag is set).
                    if cap.notify_mult != 0 {
                        notify_mult = cap.notify_mult;
                    }
                }
                pci::CAP_CFG_ISR => isr = region,
                pci::CAP_CFG_DEVICE => device_cfg = region,
                _ => {}
            }
        }
        if common == 0 || isr == 0 {
            sys::log(1, "virtio-pci: missing common/isr region");
            return None;
        }
        {
            let mut s = tanix_libsys::fmt::StrBuf::new();
            s.push_str("virtio-pci: bar4=");
            s.push_hex32(mapped_bars[4] as u32);
            s.push_str(" common=");
            s.push_hex32(common as u32);
            s.push_str(" isr=");
            s.push_hex32(isr as u32);
            s.push_str(" dev=");
            s.push_hex32(device_cfg as u32);
            s.push_str(" notify=");
            s.push_hex32(notify as u32);
            sys::log(0, s.as_str());
        }

        let dev = VirtioPci { bdf, common, notify, isr, device_cfg, irq, notify_mult };
        Some(dev)
    }

    // ── Common config access ────────────────────────────────────────────────

    #[inline]
    fn rd8(&self, off: usize) -> u8 {
        unsafe { ptr::read_volatile((self.common + off) as *const u8) }
    }
    #[inline]
    fn wr8(&self, off: usize, v: u8) {
        unsafe { ptr::write_volatile((self.common + off) as *mut u8, v) }
    }
    #[inline]
    fn rd16(&self, off: usize) -> u16 {
        unsafe { ptr::read_volatile((self.common + off) as *const u16) }
    }
    #[inline]
    fn wr16(&self, off: usize, v: u16) {
        unsafe { ptr::write_volatile((self.common + off) as *mut u16, v) }
    }
    #[inline]
    fn rd32(&self, off: usize) -> u32 {
        unsafe { ptr::read_volatile((self.common + off) as *const u32) }
    }
    #[inline]
    fn wr32(&self, off: usize, v: u32) {
        unsafe { ptr::write_volatile((self.common + off) as *mut u32, v) }
    }

    // ── Device lifecycle ────────────────────────────────────────────────────

    /// Read the full 64-bit feature set the device offers.
    pub fn device_features(&self) -> u64 {
        self.wr32(C_DEVICE_FEATURE_SEL, 0);
        let lo = self.rd32(C_DEVICE_FEATURE);
        self.wr32(C_DEVICE_FEATURE_SEL, 1);
        let hi = self.rd32(C_DEVICE_FEATURE);
        lo as u64 | ((hi as u64) << 32)
    }

    /// Write the negotiated features (both 32-bit halves).
    pub fn set_driver_features(&self, feats: u64) {
        self.wr32(C_DRIVER_FEATURE_SEL, 0);
        self.wr32(C_DRIVER_FEATURE, feats as u32);
        self.wr32(C_DRIVER_FEATURE_SEL, 1);
        self.wr32(C_DRIVER_FEATURE, (feats >> 32) as u32);
    }

    fn status(&self) -> u8 {
        self.rd8(C_DEVICE_STATUS)
    }

    fn set_status(&self, v: u8) {
        self.wr8(C_DEVICE_STATUS, v);
    }

    /// Reset the device (status = 0), then ACKNOWLEDGE | DRIVER.
    pub fn reset(&self) {
        self.set_status(0);
        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    }

    /// Negotiate features: accept `wanted ∩ offered`, then complete the
    /// FEATURES_OK step.  Returns the accepted feature set, or None if the
    /// device rejected the negotiation.
    pub fn negotiate(&self, wanted: u64) -> Option<u64> {
        let accepted = wanted & self.device_features();
        self.set_driver_features(accepted);
        self.set_status(self.status() | STATUS_FEATURES_OK);
        if self.status() & STATUS_FEATURES_OK == 0 {
            sys::log(1, "virtio-pci: FEATURES_OK rejected");
            return None;
        }
        Some(accepted)
    }

    /// Bring the device to DRIVER_OK (live).
    pub fn driver_ok(&self) {
        self.set_status(self.status() | STATUS_DRIVER_OK);
    }

    /// Enable queue `idx` backed by `ring`; returns the queue size the
    /// device accepted (0 = failure).
    pub fn setup_queue(&self, idx: u16, ring: &Vring) -> u16 {
        self.wr16(C_QUEUE_SELECT, idx);
        let size = self.rd16(C_QUEUE_SIZE); // capacity; write our size
        let n = size.min(crate::vring::QUEUE_SIZE);
        self.wr16(C_QUEUE_SIZE, n);
        self.wr64(C_QUEUE_DESC, ring.desc_base);
        self.wr64(C_QUEUE_DRIVER, ring.avail_base);
        self.wr64(C_QUEUE_DEVICE, ring.used_base);
        self.wr16(C_QUEUE_ENABLE, 1);
        let got = self.rd16(C_QUEUE_SIZE);
        if got == 0 {
            sys::log(1, "virtio-pci: queue enable failed");
            return 0;
        }
        got
    }

    fn wr64(&self, off: usize, v: u64) {
        unsafe { ptr::write_volatile((self.common + off) as *mut u64, v) }
    }

    /// Kick queue `idx` (write the queue index into its notify slot).
    ///
    /// Note: this writes the *notify* region directly — the `wr16` helpers
    /// all target the common-config region (they fold in `self.common`).
    pub fn notify(&self, idx: u16) {
        unsafe { ptr::write_volatile(self.notify_offset(idx) as *mut u16, idx) }
    }

    fn notify_offset(&self, idx: u16) -> usize {
        self.notify + idx as usize * self.notify_mult as usize
    }

    /// Read-and-clear the ISR status (deasserts the INTx line).  Returns
    /// the bits that were pending (1 = queue used, 2 = config change).
    pub fn read_isr(&self) -> u32 {
        unsafe { ptr::read_volatile(self.isr as *const u32) }
    }

    /// Device-config region helpers (virtio-net reads its MAC/status
    /// through here).
    pub fn cfg_read8(&self, off: usize) -> u8 {
        unsafe { ptr::read_volatile((self.device_cfg + off) as *const u8) }
    }

    /// Log a failure with the device's own tag.
    pub fn fail(&self, msg: &str) {
        self.set_status(self.status() | STATUS_FAILED);
        sys::log(1, msg);
    }
}
