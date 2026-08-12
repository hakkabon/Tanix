//! Board / machine abstraction — Phase 16.
//!
//! Tanix boots two QEMU machines:
//!
//!   • `virt` (default): 256 MiB DDR at 0x4000_0000, GICv3 at 0x0800_0000,
//!     PL011 at 0x0900_0000, virtio-mmio transports at 0x0A00_0000,
//!     PCIe ECAM at 0x3F00_0000.  QEMU emulates PSCI in its own EL3
//!     firmware; with `virtualization=on` the kernel starts at EL2 and no
//!     EL3 monitor is needed.
//!
//!   • `sbsa-ref` (feature `sbsa-ref`): the "real hardware" platform.
//!     CPUs reset at EL3, QEMU's PSCI is *disabled* (the platform expects
//!     the EL3 firmware to supply it — Tanix's EL3 monitor does), RAM
//!     starts at 0x100_0000_0000 (1 TiB), GICv3 distributor/redistributor
//!     at 0x4006_0000 / 0x4008_0000, PL011 at 0x6000_0000, a *secure*
//!     PL011 at 0x6003_0000 (second `-serial` chardev), 512 MiB of
//!     secure-only RAM at 0x2000_0000, PCIe ECAM at 0xF000_0000 with the
//!     32-bit MMIO window at 0x8000_0000.
//!
//! DRAM base and size come from the flattened device tree (x0 at boot)
//! where possible; the table below is the fallback when no DT is passed.
//! `secure_ram_base == 0` means the machine has no TrustZone-partitioned
//! secure RAM (QEMU `virt`) and the secure world payload runs in place.

#![allow(dead_code)]

/// Machine identifiers published to server tasks (BootInfo.machine).
pub const MACHINE_VIRT: u32 = 0;
pub const MACHINE_SBSA_REF: u32 = 1;

/// What the secure world may print to.  On `virt` this equals the NS UART
/// (the machine has a single PL011); on `sbsa-ref` it is the dedicated
/// secure console (QEMU `-serial` #2, e.g. `-serial file:sec.log`).
#[derive(Clone, Copy)]
pub struct Machine {
    pub id: u32,
    /// DRAM base (fallback when no DT was passed at boot).
    pub dram_base: usize,
    /// DRAM size (fallback when no DT was passed at boot).
    pub dram_size: usize,
    /// NS PL011 (kernel log).
    pub uart_base: usize,
    /// Secure PL011 (EL3 monitor / secure world).
    pub secure_uart_base: usize,
    /// GICv3 distributor.
    pub gic_dist_base: usize,
    /// GICv3 first redistributor (stride below).
    pub gic_redist_base: usize,
    pub gic_redist_stride: usize,
    /// QEMU `virt` virtio-mmio transport window (0 = none).
    pub virtio_mmio_base: usize,
    /// TrustZone secure RAM (0 = none — secure payload runs in place).
    pub secure_ram_base: usize,
    pub secure_ram_size: usize,
}

/// The machine this kernel was built for.
pub fn machine() -> Machine {
    #[cfg(feature = "sbsa-ref")]
    {
        Machine {
            id: MACHINE_SBSA_REF,
            dram_base: 0x100_0000_0000,
            dram_size: 1 * 1024 * 1024 * 1024,
            uart_base: 0x6000_0000,
            secure_uart_base: 0x6003_0000,
            gic_dist_base: 0x4006_0000,
            gic_redist_base: 0x4008_0000,
            gic_redist_stride: 0x2_0000,
            virtio_mmio_base: 0,
            secure_ram_base: 0x2000_0000,
            secure_ram_size: 512 * 1024 * 1024,
        }
    }
    #[cfg(not(feature = "sbsa-ref"))]
    {
        Machine {
            id: MACHINE_VIRT,
            dram_base: 0x4000_0000,
            dram_size: 256 * 1024 * 1024,
            uart_base: 0x0900_0000,
            secure_uart_base: 0x0900_0000,
            gic_dist_base: 0x0800_0000,
            gic_redist_base: 0x080A_0000,
            gic_redist_stride: 0x2_0000,
            virtio_mmio_base: 0x0A00_0000,
            secure_ram_base: 0,
            secure_ram_size: 0,
        }
    }
}

/// True when this build targets the SBSA reference platform.
pub fn is_sbsa_ref() -> bool {
    machine().id == MACHINE_SBSA_REF
}
