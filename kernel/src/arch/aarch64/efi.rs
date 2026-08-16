#![allow(dead_code)]
//! UEFI boot handoff — Phase 18.
//!
//! When Tanix is loaded as an EFI application (PE/COFF image), the UEFI
//! firmware starts the kernel at EL2 (or EL1) with:
//!
//!   x0 = image handle, x1 = pointer to the EFI system table
//!
//! The `_start` stub (main.rs) stashes x1 into `EFI_SYSTAB` before it drops
//! to EL1; under the old `-kernel` boot the register is 0 and nothing is
//! stashed.  This module turns that pointer into what the kernel actually
//! wants from the firmware:
//!
//!   • the **RSDP** of the ACPI tables (ACPI 2.0 config-table GUID), which
//!     `acpi::probe` turns into the machine topology, and
//!   • the **flattened device tree** pointer (FDT config-table GUID) when
//!     the firmware publishes one — QEMU hands its `-machine` DT to edk2,
//!     which exposes it here; the RAM layout is then read from the DT
//!     exactly like a `-kernel` boot.
//!
//! Everything is read through the system table after the kernel's own MMU
//! table is up, so firmware memory stays mapped by the identity map.

use core::ptr::{read_volatile, write_volatile};

/// Stash slot written by `_start` (assembly) at EL2/EL1 entry: the EFI
/// system table pointer, or 0 when booted with `-kernel`.  Lives in `.data`
/// (not BSS — BSS is zeroed after `_start` has already written it).
#[no_mangle]
pub static mut EFI_SYSTAB: usize = 0;

/// What the kernel needs to know about a firmware boot.
#[derive(Clone, Copy, Debug)]
pub struct EfiHandoff {
    /// True when the firmware (UEFI) started the kernel.
    pub booted_from_efi: bool,
    /// The EFI system table pointer (0 when not booted by firmware).
    pub systab: usize,
    /// RSDP address of the ACPI tables (0 = none published).
    pub rsdp: usize,
    /// Flattened device tree pointer from the firmware (0 = none).
    pub dtb: usize,
}

/// The 16-byte EFI GUID of the ACPI 2.0 configuration table:
/// 8868E871-E4F1-11D3-BC22-0080C73C8881 (little-endian byte order).
const GUID_ACPI20: [u8; 16] = [
    0x71, 0xE8, 0x68, 0x88, 0xF1, 0xE4, 0xD3, 0x11, 0xBC, 0x22, 0x00, 0x80,
    0xC7, 0x3C, 0x88, 0x81,
];

/// ACPI 1.0 table GUID: EB9D2D30-2D88-11D3-9A16-0090273FC14D.  We only use
/// it as a fallback — ACPI 1.0 has no XSDT, which `acpi::probe` requires.
const GUID_ACPI10: [u8; 16] = [
    0x30, 0x2D, 0x9D, 0xEB, 0x88, 0x2D, 0xD3, 0x11, 0x9A, 0x16, 0x00, 0x90,
    0x27, 0x3F, 0xC1, 0x4D,
];

/// Flattened device tree table GUID: B1B621D5-F19C-41A5-830B-D9152C69AAEA.
const GUID_FDT: [u8; 16] = [
    0xD5, 0x21, 0xB6, 0xB1, 0x9C, 0xF1, 0xA5, 0x41, 0x83, 0x0B, 0xD9, 0x15,
    0x2C, 0x69, 0xAA, 0xEA,
];

/// EFI_SYSTEM_TABLE.Signature — the 8 bytes "IBI SYSTEM".
const ST_SIG: u64 = 0x5459_5353_5F49_4249; // 'I''B''I'' ''S''Y''S''T''E''M' little-endian

/// Read the firmware boot state from the stash left by `_start`.
///
/// Called before anything else in `kmain` (before the UART is up, so no
/// logging here — the caller logs).
pub fn handoff() -> EfiHandoff {
    let systab = unsafe { EFI_SYSTAB };
    if systab == 0 {
        return EfiHandoff {
            booted_from_efi: false,
            systab: 0,
            rsdp: 0,
            dtb: 0,
        };
    }

    // Validate the system-table signature so a garbage stash can't be
    // misinterpreted as a firmware boot.
    let sig = unsafe { read_volatile(systab as *const u64) };
    if sig != ST_SIG {
        return EfiHandoff {
            booted_from_efi: false,
            systab: 0,
            rsdp: 0,
            dtb: 0,
        };
    }

    // Layout: signature(8) revision(4) headersize(4) crc(4) reserved(4)
    // vendor(8) fwrev(4) pad(4) conin_handle(8) conin(8) conout_handle(8)
    // conout(8) stderr_handle(8) stderr(8) runtime(8) boot_services(8)
    // n_entries(4) pad(4) then n_entries * { guid(16), ptr(8) }.
    let n_entries = unsafe { read_volatile((systab + 112) as *const u32) } as usize;
    let cfg = systab + 120;
    if n_entries > 64 {
        // Firmware claiming absurd table counts is not one we trust.
        return EfiHandoff {
            booted_from_efi: true,
            systab,
            rsdp: 0,
            dtb: 0,
        };
    }

    let mut rsdp = 0usize;
    let mut dtb = 0usize;
    for i in 0..n_entries {
        let e = cfg + i * 24;
        let guid = unsafe { &*(e as *const [u8; 16]) };
        let ptr = unsafe { read_volatile((e + 16) as *const u64) } as usize;
        if ptr == 0 {
            continue;
        }
        if guid == &GUID_ACPI20 && rsdp == 0 {
            rsdp = ptr;
        } else if guid == &GUID_ACPI10 && rsdp == 0 {
            rsdp = ptr;
        } else if guid == &GUID_FDT && dtb == 0 {
            dtb = ptr;
        }
    }

    EfiHandoff {
        booted_from_efi: true,
        systab,
        rsdp,
        dtb,
    }
}

/// (Debug helper) record a non-EFI boot explicitly.
pub fn mark_direct_boot() {
    unsafe {
        write_volatile(&raw mut EFI_SYSTAB, 0);
    }
}
