//! Minimal virtio-mmio (legacy, virtio 0.9.5) transport driver.
//!
//! QEMU's `virt` machine exposes 32 virtio-mmio transports at 0x0A00_0000,
//! 0x200 apart; `-device virtio-gpu-device` / `virtio-tablet-device` attach
//! to the *last* free buses (confirmed via `info mtree`/`info qtree`).
//! The transports are legacy-only (`force-legacy=true`, version register =
//! 1), so this driver uses the old interface: a single-page-aligned vring
//! addressed by physical frame number (`QueuePFN`).
//!
//! Everything here is plain MMIO (device-nGnRnE, pre-mapped by the kernel).
//! Transfers are interrupt-driven (Phase 7): after kicking a queue the
//! driver blocks on `SYS_WAIT_IRQ` for the device's interrupt (SPI 48+slot,
//! level-triggered), acknowledges it via INT_STATUS/INT_ACK, and then
//! drains the used ring.  The poll loop below is kept as a fallback for
//! requests that completed before the wait returned.

use core::ptr;

use tanix_libsys::sys;

/// Base of the first virtio-mmio slot (see kernel page_table.rs).
pub const MMIO_BASE: usize = 0x0A00_0000;
/// Slot stride: QEMU lays 32 transport windows of 0x200 bytes back-to-back.
pub const SLOT_SIZE: usize = 0x200;
/// Number of slots to probe.
pub const SLOTS: usize = 32;
/// First interrupt the virtio-mmio devices use (see kernel gic.rs).
pub const IRQ_BASE: u32 = 48;

// Transport registers (virtio-mmio; QEMU hw/virtio/virtio-mmio.h).  This
// build of QEMU shares one register map between legacy and modern modes:
// offsets match both except QUEUE_NOTIFY (0x050, not the old draft's 0x048).
const REG_MAGIC: usize = 0x000;
const REG_DEVICE_ID: usize = 0x008;
const REG_HOST_FEATURES: usize = 0x010;
const REG_HOST_FEATURES_SEL: usize = 0x014;
const REG_GUEST_FEATURES: usize = 0x020;
const REG_GUEST_FEATURES_SEL: usize = 0x024;
const REG_GUEST_PAGE_SIZE: usize = 0x028;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_ALIGN: usize = 0x03C;
const REG_QUEUE_PFN: usize = 0x040;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_INTERRUPT_STATUS: usize = 0x060;
const REG_INTERRUPT_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;

const MAGIC_VALUE: u32 = 0x7472_6976; // "virt" (see note on legacy layout)

// Device status bits (virtio 0.9.5: no FEATURES_OK in legacy).
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;

/// Ring descriptor flags.
const DESC_WRITE: u16 = 2; // device writes into this buffer
const DESC_NEXT: u16 = 1;

pub const QUEUE_SIZE: usize = 64;
const VRING_ALIGN: usize = 4096;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UsedElem {
    id: u32,
    len: u32,
}

/// A legacy vring laid out per QEMU 11 `virtio_queue_update_rings`
/// (hw/virtio/virtio.c): the avail ring follows the descriptor table
/// immediately, the used ring sits at the next 4 KiB alignment:
///
///   • 0x0000 .. 0x0400 — 64 × 16 B descriptors
///   • 0x0400 .. 0x0484 — avail ring (flags + idx + 64 heads)
///   • 0x1000 .. 0x1204 — used ring (flags + idx + 64 entries)
///
/// (Older QEMU put avail at `desc + align` instead; QEMU 11 does not.)
/// The region spans two contiguous frames.
#[repr(C, align(4096))]
struct Vring {
    desc: [Desc; QUEUE_SIZE],
    avail_flags: u16,
    avail_idx: u16,
    avail_ring: [u16; QUEUE_SIZE],
    _pad: [u8; 0x1000 - 0x0400 - 4 - QUEUE_SIZE * 2],
    used_flags: u16,
    used_idx: u16,
    used_ring: [UsedElem; QUEUE_SIZE],
}

/// Size of the vring backing region in bytes (two 4 KiB frames).
const VRING_SIZE: usize = 2 * 4096;

/// A virtqueue in its two-frame backing region.
pub struct VirtQueue {
    base: u64,
    /// How far the driver has consumed the used ring.
    used_tail: usize,
    /// Next free descriptor slot.  Advances by the chain length of each
    /// submission — it is NOT `avail_idx`: two submissions sharing the
    /// same desc slot would let the device read an overwritten chain.
    desc_head: usize,
}

impl VirtQueue {
    /// Allocate the two backing frames and zero the vring.
    pub fn new() -> Option<Self> {
        let base = sys::alloc_frames(2);
        if base == 0 {
            return None;
        }
        // write_bytes counts in *element* units — cast to u8 so the count
        // is bytes, or this would zero `count × size_of::<Vring>()` bytes.
        let vring = base as *mut u8;
        unsafe {
            core::ptr::write_bytes(vring, 0, VRING_SIZE);
        }
        Some(Self { base, used_tail: 0, desc_head: 0 })
    }

}

fn vring_at(base: u64) -> &'static mut Vring {
    unsafe { &mut *(base as *mut Vring) }
}

/// One probed virtio-mmio device.
#[derive(Clone, Copy)]
pub struct Device {
    /// MMIO base address (already identity-mapped by the kernel).
    pub base: usize,
}

/// Scan all virtio-mmio slots for a device with the given id.
pub fn find(device_id: u32) -> Option<Device> {
    for i in 0..SLOTS {
        let base = MMIO_BASE + i * SLOT_SIZE;
        if read32(base, REG_MAGIC) != MAGIC_VALUE {
            continue;
        }
        let id = read32(base, REG_DEVICE_ID);
        if id == device_id {
            return Some(Device { base });
        }
    }
    None
}

impl Device {
    /// The interrupt number (SPI) this transport uses on the kernel's GIC:
    /// `48 + slot` (QEMU `virt` machine).
    pub fn irq(&self) -> u32 {
        IRQ_BASE + ((self.base - MMIO_BASE) / SLOT_SIZE) as u32
    }

    /// Acknowledge the device's interrupt.  The transport is level-triggered:
    /// reading INT_STATUS returns the pending bits; writing them back to
    /// INT_ACK deasserts the line, so the GIC's SPI drops.
    pub fn ack_interrupt(&self) {
        let status = read32(self.base, REG_INTERRUPT_STATUS);
        write32(self.base, REG_INTERRUPT_ACK, status);
    }

    /// Reset the device: status = 0, then ACKNOWLEDGE | DRIVER.
    pub fn reset(&self) {
        write32(self.base, REG_STATUS, 0);
        write32(self.base, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    }

    /// Read the offered (host) features.
    pub fn host_features(&self) -> u32 {
        write32(self.base, REG_HOST_FEATURES_SEL, 0);
        read32(self.base, REG_HOST_FEATURES)
    }

    /// Write the negotiated guest features (legacy: no FEATURES_OK step —
    /// DRIVER_OK afterwards means "features are settled").
    pub fn negotiate(&self, features: u32) {
        write32(self.base, REG_GUEST_FEATURES_SEL, 0);
        write32(self.base, REG_GUEST_FEATURES, features);
    }

    /// Bring the device to DRIVER_OK (live).
    pub fn driver_ok(&self) {
        write32(self.base, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    }

    /// Set up queue `queue_idx` in the given vring.  Returns the size.
    pub fn setup_queue(&self, queue_idx: u32, queue: &mut VirtQueue) -> Option<u32> {
        write32(self.base, REG_GUEST_PAGE_SIZE, 4096);
        write32(self.base, REG_QUEUE_SEL, queue_idx);
        let max = read32(self.base, REG_QUEUE_NUM_MAX);
        if max == 0 {
            let mut buf = tanix_libsys::fmt::StrBuf::new();
            buf.push_str("virtio: queue ");
            buf.push_dec32(queue_idx);
            buf.push_str(" num_max=0");
            sys::log(1, buf.as_str());
            return None;
        }
        let n = max.min(QUEUE_SIZE as u32);
        write32(self.base, REG_QUEUE_NUM, n);
        write32(self.base, REG_QUEUE_ALIGN, VRING_ALIGN as u32);
        write32(self.base, REG_QUEUE_PFN, (queue.base >> 12) as u32);
        Some(n)
    }

    /// Submit a request: `request` is one or more READ buffers forming a
    /// chain, `response` is the final WRITE buffer.
    ///
    /// The caller must keep `request`/`response` alive until this returns.
    /// Synchronous: kicks the queue, blocks on `SYS_WAIT_IRQ` until the
    /// device interrupts, acknowledges it, then drains the used ring until
    /// the chain's head descriptor is returned.  Returns the length the
    /// device wrote into `response`.
    pub fn submit(
        &self,
        queue: &mut VirtQueue,
        request: &[&[u8]],
        response: &mut [u8],
    ) -> u32 {
        debug_assert!(request.len() < QUEUE_SIZE);
        let vring = vring_at(queue.base);
        let idx = vring.avail_idx as usize;
        let head = queue.desc_head;

        // Chain: request buffers (READ, DESC_NEXT) → response (WRITE).
        for (i, buf) in request.iter().enumerate() {
            let slot = (head + i) % QUEUE_SIZE;
            let next = (head + i + 1) % QUEUE_SIZE;
            vring.desc[slot] = Desc {
                addr: buf.as_ptr() as u64,
                len: buf.len() as u32,
                flags: DESC_NEXT,
                next: next as u16,
            };
        }
        let resp_slot = (head + request.len()) % QUEUE_SIZE;
        vring.desc[resp_slot] = Desc {
            addr: response.as_ptr() as u64,
            len: response.len() as u32,
            flags: DESC_WRITE,
            next: 0,
        };
        queue.desc_head = (head + request.len() + 1) % QUEUE_SIZE;

        // Publish the head descriptor in the avail ring.
        vring.avail_ring[idx % QUEUE_SIZE] = head as u16;
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        vring.avail_idx = vring.avail_idx.wrapping_add(1);

        // Kick.
        write32(self.base, REG_QUEUE_NOTIFY, 0);

        // Phase 7: block until the device raises its interrupt (the kernel
        // enables the SPI on first wait, records the pending edge, and
        // wakes us), then deassert it.  Level-triggered, so a completion
        // that beat the wait still leaves the line high → no lost wakeup.
        let irq = self.irq();
        if sys::wait_irq(irq) >= 0 {
            self.ack_interrupt();
        }

        // Poll until the device completes our head descriptor.
        let mut used_tail = queue.used_tail; // copy before vring borrow
        loop {
            let used_idx = unsafe { ptr::read_volatile(&vring.used_idx) } as usize;
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            while used_tail < used_idx {
                let elem = &vring.used_ring[used_tail % QUEUE_SIZE];
                used_tail += 1;
                if elem.id as usize == head {
                    queue.used_tail = used_tail;
                    return elem.len;
                }
            }
        }
    }

    /// Add `count` independent device-writable buffers at `base + i*len` to
    /// the avail ring and kick.
    ///
    /// Event-queue pattern: the device fills the buffers asynchronously and
    /// returns them via the used ring; the driver re-adds each buffer after
    /// draining its used entry.  Returns the number added (0 when the avail
    /// ring has no room).
    pub fn add_empty_buffers(
        &self,
        queue: &mut VirtQueue,
        base: *mut u8,
        len: u32,
        count: usize,
    ) -> usize {
        let vring = vring_at(queue.base);
        let used_idx = unsafe { ptr::read_volatile(&vring.used_idx) } as usize;
        let avail_idx = vring.avail_idx as usize;
        if avail_idx - used_idx + count > QUEUE_SIZE {
            return 0;
        }
        for i in 0..count {
            let slot = (queue.desc_head + i) % QUEUE_SIZE;
            vring.desc[slot] = Desc {
                addr: base as u64 + (i as u64 * len as u64),
                len,
                flags: DESC_WRITE,
                next: 0,
            };
            vring.avail_ring[(avail_idx + i) % QUEUE_SIZE] = slot as u16;
        }
        queue.desc_head = (queue.desc_head + count) % QUEUE_SIZE;
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        vring.avail_idx = (avail_idx + count) as u16;
        write32(self.base, REG_QUEUE_NOTIFY, 0);
        count
    }

    /// Drain completed used-ring entries (descriptor id, length) into `out`,
    /// oldest first.  Returns how many entries were written.
    pub fn drain_used(&self, queue: &mut VirtQueue, out: &mut [(u16, u32)]) -> usize {
        let vring = vring_at(queue.base);
        let used_idx = unsafe { ptr::read_volatile(&vring.used_idx) } as usize;
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let mut n = 0;
        while queue.used_tail < used_idx && n < out.len() {
            let elem = &vring.used_ring[queue.used_tail % QUEUE_SIZE];
            out[n] = (elem.id as u16, elem.len);
            queue.used_tail += 1;
            n += 1;
        }
        n
    }
}

#[inline]
fn read32(base: usize, reg: usize) -> u32 {
    unsafe { ptr::read_volatile((base + reg) as *const u32) }
}

#[inline]
fn write32(base: usize, reg: usize, val: u32) {
    unsafe { ptr::write_volatile((base + reg) as *mut u32, val) }
}
