//! Tanix Phase-10 device-driver library.
//!
//! A small no_std driver stack for the QEMU `virt` machine, used by the
//! `net` server:
//!
//!   • `pci`       — PCIe ECAM config-space access (QEMU `virt,highmem=off`:
//!                   ECAM at 0x3F00_0000), device scan, BARs, capability
//!                   walking, virtio vendor capabilities, INTx→GIC SPI
//!                   swizzling.
//!   • `vring`     — a modern (virtio 1.0) split vring in three frames.
//!   • `virtio_pci`— the modern virtio-pci transport: capability-driven
//!                   region discovery, 64-bit feature negotiation with the
//!                   FEATURES_OK step, device status, per-queue setup over
//!                   the common config, queue notification, ISR handling.
//!   • `net`       — the virtio-net driver on top (device id 1, RX/TX
//!                   queues, the 12-byte virtio_net_hdr).
//!
//! The transport runs **interrupt-driven**: the virtio-pci device asserts
//! its legacy INTx line (SPI 35..38 depending on the PCI slot) for every
//! used-ring update; the server consumes the recorded interrupt with
//! `SYS_IRQ_PENDING` and deasserts the line by reading the ISR register.
//!
//! Everything is plain identity-mapped MMIO: the driver maps the windows
//! it needs (ECAM, device BARs) with `SYS_MAP_DEVICE`, which also makes
//! them visible in the kernel's own table.

#![no_std]

pub mod net;
pub mod pci;
pub mod virtio_pci;
pub mod vring;
