//! Board / machine abstraction — Phase 16; extended Phase 23 (QCS6490).
//!
//! Tanix boots on three configurations:
//!
//!   • `virt` (default): 256 MiB DDR at 0x4000_0000, GICv3 at 0x0800_0000,
//!     PL011 at 0x0900_0000, virtio-mmio transports at 0x0A00_0000,
//!     PCIe ECAM at 0x3F00_0000.  QEMU emulates PSCI in its own EL3
//!     firmware; with `virtualization=on` the kernel starts at EL2 and no
//!     EL3 monitor is needed.
//!
//!   • `sbsa-ref` (feature `sbsa-ref`): the "real hardware" QEMU platform.
//!     CPUs reset at EL3, QEMU's PSCI is *disabled* (the platform expects
//!     the EL3 firmware to supply it — Tanix's EL3 monitor does), RAM
//!     starts at 0x100_0000_0000 (1 TiB), GICv3 distributor/redistributor
//!     at 0x4006_0000 / 0x4008_0000, PL011 at 0x6000_0000, a *secure*
//!     PL011 at 0x6003_0000 (second `-serial` chardev), 512 MiB of
//!     secure-only RAM at 0x2000_0000, PCIe ECAM at 0xF000_0000 with the
//!     32-bit MMIO window at 0x8000_0000.
//!
//!   • `qcs6490` (feature `qcs6490`): Phase 23 real silicon target —
//!     Radxa Dragon Q6A and RUBIK Pi 3, both carrying a Qualcomm Dragonwing
//!     QCS6490 SoC.  Register addresses are derived from the upstream kernel
//!     DTS (`arch/arm64/boot/dts/qcom/qcs6490.dtsi`):
//!       - DRAM: 8 GiB @ 0x8000_0000 (board typically has 8 GiB; UEFI/ACPI
//!               refines the exact range at boot via `set_from_acpi`).
//!       - UART: GENI SE UART0 @ 0x00A9_0000 (ttyMSM0, 115200 n8).
//!       - GICv3 distributor @ 0x17A0_0000, first redistributor @ 0x17A6_0000
//!         (stride 0x2_0000 per CPU, 8 CPUs on QCS6490).
//!       - PCIe ECAM: host controller 0 @ 0x01FC_0000 (PCIe ECAM config
//!         window per ACPI / QCOM PCIe DT node; refined at boot).
//!       - No `virtio_mmio_base` (real hardware, no QEMU transport).
//!       - No in-kernel EL3 monitor: QSEE already occupies EL3 on stock
//!         firmware.  The kernel enters at NS EL1 via UEFI.
//!
//! DRAM base and size come from the flattened device tree (x0 at boot)
//! where possible; the table below is the fallback when no DT is passed.
//! `secure_ram_base == 0` means the machine has no TrustZone-partitioned
//! secure RAM (QEMU `virt`) and the secure world payload runs in place.

#![allow(dead_code)]

/// Machine identifiers published to server tasks (BootInfo.machine).
pub const MACHINE_VIRT: u32 = 0;
pub const MACHINE_SBSA_REF: u32 = 1;
/// Phase 23: Qualcomm Dragonwing QCS6490 real silicon
/// (Dragon Q6A, RUBIK Pi 3).
pub const MACHINE_QCS6490: u32 = 2;

/// What the secure world may print to.  On `virt` this equals the NS UART
/// (the machine has a single PL011); on `sbsa-ref` it is the dedicated
/// secure console (QEMU `-serial` #2, e.g. `-serial file:sec.log`);
/// on `qcs6490` there is no in-kernel secure console (QSEE owns EL3).
#[derive(Clone, Copy)]
pub struct Machine {
    pub id: u32,
    /// DRAM base (fallback when no DT was passed at boot).
    pub dram_base: usize,
    /// DRAM size (fallback when no DT was passed at boot).
    pub dram_size: usize,
    /// NS UART base — PL011 on `virt`/`sbsa-ref`, GENI SE on `qcs6490`.
    pub uart_base: usize,
    /// Secure PL011 (EL3 monitor / secure world); 0 on `qcs6490`.
    pub secure_uart_base: usize,
    /// GICv3 distributor.
    pub gic_dist_base: usize,
    /// GICv3 first redistributor (stride below).
    pub gic_redist_base: usize,
    pub gic_redist_stride: usize,
    /// QEMU `virt` virtio-mmio transport window (0 = none / real hardware).
    pub virtio_mmio_base: usize,
    /// TrustZone secure RAM (0 = none — secure payload runs in place or
    /// EL3 is occupied by vendor firmware like QSEE).
    pub secure_ram_base: usize,
    pub secure_ram_size: usize,
    /// GIC ITS base (Phase 18 — MSI-X/LPI doorbells; 0 = no ITS).
    pub its_base: usize,
    /// PCIe ECAM window base (Phase 18; 0 = no PCIe).
    pub ecam_base: usize,
    /// True when the UART is a Qualcomm GENI Serial Engine (not a PL011).
    /// Drivers in `uart.rs` / `geni_uart.rs` check this to choose the
    /// correct register-level path.
    pub uart_is_geni: bool,
}

/// Phase 18: the machine the kernel was *built for* (compile-time default).
/// `machine()` may be overridden at boot with ACPI-discovered values
/// (`set_from_acpi`), so this is only the pre-firmware answer.
const fn default_machine() -> Machine {
    #[cfg(feature = "qcs6490")]
    {
        // Phase 23: Qualcomm Dragonwing QCS6490 — Dragon Q6A / RUBIK Pi 3.
        //
        // Register addresses from:
        //   arch/arm64/boot/dts/qcom/qcs6490.dtsi (Linux 6.8)
        //   arch/arm64/boot/dts/qcom/qcs6490-radxa-dragon-q6a.dts
        //
        // UEFI/ACPI firmware (stock Dragon Q6A EDK2) will call
        // `set_from_acpi` and refine these values at boot.  These are
        // the DTS defaults that serve as a safe fallback when no ACPI
        // tables are found (e.g. bare-metal EL1 entry without UEFI).
        Machine {
            id: MACHINE_QCS6490,
            // QCS6490 boards typically have 8 GiB LPDDR5, starting at 0x80000000.
            // The first 2 GiB (0x8000_0000..0xFFFF_FFFF) are the 32-bit window;
            // extended RAM is above 0x1_0000_0000.  UEFI refines this.
            dram_base: 0x8000_0000,
            dram_size: 8 * 1024 * 1024 * 1024,
            // GENI UART0 / ttyMSM0.
            // qcom,qcs6490-geni-se-qup / serial@a90000 in the DTS.
            uart_base: 0x00A9_0000,
            secure_uart_base: 0, // QSEE owns EL3; no in-kernel secure console.
            // GICv3: intc@17a00000 in qcs6490.dtsi
            //   distributor: 0x17A0_0000
            //   redistributors: 0x17A6_0000 (8 CPUs, stride 0x2_0000)
            gic_dist_base:   0x17A0_0000,
            gic_redist_base: 0x17A6_0000,
            gic_redist_stride: 0x2_0000,
            // GIC ITS: gic_its@17a40000 in qcs6490.dtsi
            its_base: 0x17A4_0000,
            // PCIe: pcie0 / pcie@01fc0000; the ECAM window address comes from
            // the `reg` property of the pcie-rc node.  ACPI MCFG overrides this.
            ecam_base: 0x01FC_0000,
            // No QEMU virtio-mmio on real silicon.
            virtio_mmio_base: 0,
            // No in-kernel TrustZone partition (QSEE owns secure world).
            secure_ram_base: 0,
            secure_ram_size: 0,
            // UART is a GENI SE, not a PL011.
            uart_is_geni: true,
        }
    }
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
            its_base: 0x4408_1000,
            ecam_base: 0xF000_0000,
            uart_is_geni: false,
        }
    }
    #[cfg(not(any(feature = "sbsa-ref", feature = "qcs6490")))]
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
            its_base: 0,
            ecam_base: 0x3F00_0000,
            uart_is_geni: false,
        }
    }
}

/// The active machine: compile-time default, replaced once at boot when
/// ACPI tables are available (Phase 18).  Single-threaded boot-time write
/// — every reader uses `machine()`.
static mut CURRENT: Machine = default_machine();

/// The machine this kernel runs on.
pub fn machine() -> Machine {
    unsafe { CURRENT }
}

/// Phase 18: override the compile-time machine with values parsed from the
/// ACPI tables published by the UEFI firmware.  Only non-zero ACPI values
/// win; everything else keeps the build-time default.  Must be called
/// before any hardware init (single-threaded boot phase).
pub fn set_from_acpi(info: &crate::arch::aarch64::acpi::AcpiInfo) {
    let mut m = machine();
    if info.gic_dist_base != 0 {
        m.gic_dist_base = info.gic_dist_base;
    }
    if info.gic_redist_base != 0 {
        m.gic_redist_base = info.gic_redist_base;
    }
    if info.its_base != 0 {
        m.its_base = info.its_base;
    }
    if info.uart_base != 0 {
        m.uart_base = info.uart_base;
        // On qcs6490 the ACPI SPCR points at the GENI SE base — keep
        // `uart_is_geni` as the build-time value (already true for that
        // target).  On virt/sbsa-ref SPCR points at a PL011 and
        // `uart_is_geni` remains false.
        if m.id != MACHINE_QCS6490 {
            m.secure_uart_base = info.uart_base;
        }
    }
    if info.ecam_base != 0 {
        m.ecam_base = info.ecam_base;
    }
    unsafe { CURRENT = m };
}

/// True when this build targets the SBSA reference platform.
pub fn is_sbsa_ref() -> bool {
    machine().id == MACHINE_SBSA_REF
}

/// True when this build targets QCS6490 real silicon
/// (Dragon Q6A, RUBIK Pi 3).
pub fn is_qcs6490() -> bool {
    machine().id == MACHINE_QCS6490
}
