//! PCIe ECAM config-space driver — QEMU `virt` machine (`highmem=off`).
//!
//! With `highmem=off` the machine places the PCIe ECAM window at
//! `0x3F00_0000` (16 MiB → 16 buses), clear of the 256 MiB DDR that
//! starts at 0x4000_0000.  (On this QEMU's default `highmem` layout the
//! ECAM window overlaps the RAM window and the RAM wins, so PCI config
//! space is unreachable — the machine must run with `highmem=off`.)
//!
//! ECAM addressing: config space of device `(bus, dev, func)` sits at
//! `ECAM_BASE + (bus << 20) | (dev << 15) | (func << 12)`.  All accesses
//! are volatile MMIO reads/writes through a window the caller maps with
//! `SYS_MAP_DEVICE`.

use core::ptr;

use tanix_libsys::sys;

/// ECAM window base (QEMU `virt,highmem=off`).
pub const ECAM_BASE: usize = 0x3F00_0000;
/// ECAM window size: 16 MiB = 16 buses × 1 MiB.
pub const ECAM_SIZE: usize = 16 * 1024 * 1024;

/// Red Hat / Qumranet vendor id — every virtio PCI device.
pub const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Modern-only virtio device ids: `0x1040 + device_id` (virtio 1.0);
/// transitional devices keep the legacy `0x1000 + device_id`.
pub const VIRTIO_DEVICE_MODERN_BASE: u16 = 0x1040;
pub const VIRTIO_DEVICE_LEGACY_BASE: u16 = 0x1000;

/// Virtio vendor capability (PCI_CAP_ID_VNDR = 0x09) configuration types.
pub const CAP_CFG_COMMON: u8 = 0x1;
pub const CAP_CFG_NOTIFY: u8 = 0x2;
pub const CAP_CFG_ISR: u8 = 0x3;
pub const CAP_CFG_DEVICE: u8 = 0x4;
pub const CAP_CFG_PCI: u8 = 0x5;

/// A (bus, device, function) triplet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Bdf(pub u8, pub u8, pub u8);

/// One discovered virtio capability: which BAR, where inside it, and how
/// large the region is (size 0 = use the BAR size).  `notify_mult` is the
/// per-queue byte stride from the notify capability's
/// `notify_off_multiplier` field (0 when the cap was too short to carry
/// one, e.g. for the other three region types).
#[derive(Clone, Copy)]
pub struct VirtioCap {
    pub cfg_type: u8,
    pub bar: u8,
    pub offset: u32,
    pub size: u32,
    pub notify_mult: u32,
}

// ── ECAM access ──────────────────────────────────────────────────────────────

#[inline]
fn cfg_addr(b: Bdf, off: usize) -> usize {
    ECAM_BASE
        | ((b.0 as usize) << 20)
        | ((b.1 as usize) << 15)
        | ((b.2 as usize) << 12)
        | off
}

pub fn read8(b: Bdf, off: usize) -> u8 {
    unsafe { ptr::read_volatile(cfg_addr(b, off) as *const u8) }
}

pub fn read16(b: Bdf, off: usize) -> u16 {
    unsafe { ptr::read_volatile(cfg_addr(b, off) as *const u16) }
}

pub fn read32(b: Bdf, off: usize) -> u32 {
    unsafe { ptr::read_volatile(cfg_addr(b, off) as *const u32) }
}

pub fn write16(b: Bdf, off: usize, val: u16) {
    unsafe { ptr::write_volatile(cfg_addr(b, off) as *mut u16, val) }
}

pub fn write32(b: Bdf, off: usize, val: u32) {
    unsafe { ptr::write_volatile(cfg_addr(b, off) as *mut u32, val) }
}

/// Command-register bits (PCI_COMMAND, offset 0x04).
pub const CMD_MEMORY: u16 = 1 << 1; // respond to memory BAR space
pub const CMD_MASTER: u16 = 1 << 2; // bus mastering (DMA)

/// Enable a device: set the command-register bits that make QEMU map the
/// memory BARs and allow DMA.  Without `CMD_MEMORY` QEMU refuses to
/// map any BAR (`pci_bar_address` → `PCI_BAR_UNMAPPED`) and the MMIO
/// reads return 0xFF.
pub fn enable_device(b: Bdf) {
    let cmd = read16(b, 0x04);
    write16(b, 0x04, cmd | CMD_MEMORY | CMD_MASTER);
}

// ── Header fields ────────────────────────────────────────────────────────────

pub fn vendor(b: Bdf) -> u16 {
    read16(b, 0x00)
}

pub fn device(b: Bdf) -> u16 {
    read16(b, 0x02)
}

/// Class code (top byte = class, e.g. 0x02 network).
pub fn class(b: Bdf) -> u8 {
    read8(b, 0x0B)
}

/// `interrupt pin` (1 = INTA .. 4 = INTD; 0 = no INTx).
pub fn interrupt_pin(b: Bdf) -> u8 {
    read8(b, 0x3D)
}

/// Base of the PCI capability list (0 = none).
pub fn cap_ptr(b: Bdf) -> u8 {
    read8(b, 0x34)
}

/// Read BAR `idx` (0..5) as a 64-bit address.  Handles 32-bit and 64-bit
/// memory BARs; I/O BARs are not expected on this platform.
pub fn bar(b: Bdf, idx: usize) -> u64 {
    let lo = read32(b, 0x10 + idx * 4);
    if lo & 0x1 != 0 {
        return (lo & 0x3) as u64; // I/O BAR — we only support MMIO
    }
    if lo & 0x4 != 0 {
        // 64-bit BAR: the next dword is the high half.
        let hi = read32(b, 0x10 + (idx + 1) * 4);
        ((hi as u64) << 32) | (lo & !0xF) as u64
    } else {
        (lo & !0xF) as u64
    }
}

/// Assign BAR `idx` to physical address `base` (guest-side enumeration).
///
/// QEMU's `virt` machine leaves a 64-bit prefetchable BAR at base 0 when
/// `highmem=off` removes the mmio64 window it would normally live in —
/// the gpex controller then maps nothing for it.  The driver must write
/// an address inside the 32-bit PCI window itself (standard PCI firmware
/// behaviour; QEMU routes accesses by the BAR's programmed value).
pub fn assign_bar(b: Bdf, idx: usize, base: u64) {
    let reg = 0x10 + idx * 4;
    let lo = read32(b, reg);
    if lo & 0x1 != 0 {
        return; // I/O BAR — skip
    }
    let addr_bits = (base as u32) & !0xF;
    if lo & 0x4 != 0 {
        write32(b, reg, (lo & 0xF) | addr_bits);
        write32(b, reg + 4, (base >> 32) as u32);
    } else {
        write32(b, reg, (lo & 0xF) | addr_bits);
    }
}

// ── Device scan ──────────────────────────────────────────────────────────────

/// Run `f` for every device on bus 0 with a vendor id of `0xFFFF`-less
/// (i.e. any real device, skipping the host bridge only when asked).
pub fn for_each<F: FnMut(Bdf)>(mut f: F) {
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let b = Bdf(0, dev, func);
            if vendor(b) == 0xFFFF {
                continue;
            }
            f(b);
        }
    }
}

/// Find the first device with the given vendor + (device id or class).
pub fn find(vendor_id: u16, device_id: u16) -> Option<Bdf> {
    let mut found = None;
    for_each(|b| {
        if found.is_none() && vendor(b) == vendor_id && device(b) == device_id {
            found = Some(b);
        }
    });
    found
}

/// Find a virtio device of the given device id (works for both the modern
/// `0x1040 + id` and transitional `0x1000 + id` encodings).
pub fn find_virtio(device_id: u16) -> Option<Bdf> {
    let mut found = None;
    for_each(|b| {
        if found.is_none()
            && vendor(b) == VIRTIO_VENDOR
            && (device(b) == VIRTIO_DEVICE_MODERN_BASE + device_id
                || device(b) == VIRTIO_DEVICE_LEGACY_BASE + device_id)
        {
            found = Some(b);
        }
    });
    found
}

// ── Virtio vendor capabilities ───────────────────────────────────────────────

/// Walk the capability list and collect the virtio vendor capabilities
/// into `out`.  Returns how many were stored.
pub fn virtio_caps(b: Bdf, out: &mut [VirtioCap]) -> usize {
    let mut n = 0;
    let mut ptr = cap_ptr(b);
    while ptr != 0 {
        let id = read8(b, ptr as usize);
        let next = read8(b, ptr as usize + 1);
        if id == 0x09 {
            // struct virtio_pci_cap: id, next, cap_len, cfg_type, bar,
            // padding[3], offset (le32), length (le32).
            let cap_len = read8(b, ptr as usize + 2);
            let cfg_type = read8(b, ptr as usize + 3);
            if cfg_type <= CAP_CFG_PCI {
                let bar = read8(b, ptr as usize + 4);
                let mut offset: u32 = 0;
                let mut size: u32 = 0;
                let mut notify_mult: u32 = 0;
                if cap_len >= 16 {
                    for i in 0..4 {
                        offset |= (read8(b, ptr as usize + 8 + i) as u32) << (8 * i);
                        size |= (read8(b, ptr as usize + 12 + i) as u32) << (8 * i);
                    }
                }
                if cap_len >= 20 {
                    for i in 0..4 {
                        notify_mult |= (read8(b, ptr as usize + 16 + i) as u32) << (8 * i);
                    }
                }
                if n < out.len() {
                    out[n] = VirtioCap { cfg_type, bar, offset, size, notify_mult };
                    n += 1;
                }
            }
        }
        ptr = next;
    }
    n
}

/// Map `size` bytes at physical `base` into this task's address space
/// (Device-nGnRnE, page-aligned) and return the base (identity map).
pub fn map_mmio(base: u64, size: usize) -> Result<u64, i32> {
    let pages = (size + 4095) / 4096;
    if sys::map_device(base, pages as u32) != 0 {
        return Err(-1);
    }
    Ok(base)
}

/// Map the whole ECAM region (config space of every bus) into this task's
/// address space.  Must run before any config read/write.
pub fn map_ecam() -> Result<(), i32> {
    map_mmio(ECAM_BASE as u64, ECAM_SIZE)?;
    Ok(())
}

/// The GIC SPI for a device's legacy INTx line.
///
/// QEMU `virt` routes the gpex host bridge's four INTx lines to GIC SPIs
/// `32 + VIRT_PCIE` = 35..38 (hw/arm/virt.c `a15irqmap`); a device at
/// slot `s` with interrupt pin `p` (1-based) swizzles to line
/// `(s + p - 1) % 4` (hw/pci-host/gpex.c `gpex_swizzle_map_irq_fn`).
pub fn intx_irq(b: Bdf) -> u32 {
    const PCI_IRQ_BASE: u32 = 35;
    let slot = (b.1 >> 3) as u32;
    let pin = interrupt_pin(b).max(1) as u32 - 1;
    PCI_IRQ_BASE + (slot + pin) % 4
}
