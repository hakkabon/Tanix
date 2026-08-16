#![allow(dead_code)]
//! ACPI table parsing — Phase 18.
//!
//! Tanix normally boots with a compile-time `Machine` table.  Phase 18 adds
//! a real firmware story: when booted by UEFI (edk2 on QEMU sbsa-ref /
//! virt), the firmware publishes an RSDP through the EFI configuration
//! table, and the hardware topology — GIC distributor / redistributor /
//! ITS bases, the console PL011, the PCIe ECAM window — is read from the
//! ACPI tables instead of being assumed.
//!
//! The parser is deliberately tiny and reads only the tables a bare-metal
//! kernel needs (RSDP v2 → XSDT → MADT / SPCR / MCFG).  Every pointer is
//! sanity-checked (signature + length + 8-bit checksum) so a corrupt or
//! hostile firmware can never push a wild address into the kernel.
//!
//! The tables live in RAM published by the firmware; the kernel's identity
//! map covers the whole RAM window, so the parser can dereference them
//! directly (they are mapped again by the EFI stub's stage-1 until the
//! kernel's own table replaces it).

use core::ptr::read_volatile;

// ── Checksummed table header ──────────────────────────────────────────────────

/// ACPI "common table header": signature(4) + length(4) + revision(1) +
/// checksum(1) + oem id(6) + oem table id(8) + oem revision(4) +
/// creator id(4) + creator revision(4) = 36 bytes.
#[repr(C)]
struct TableHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    _rest: [u8; 26],
}

/// Read the header at `addr` and validate signature + checksum.
/// Returns `(signature, length)` on success.
fn valid_table(addr: usize, expect: &[u8; 4]) -> Option<(u32, usize)> {
    if addr & 3 != 0 || addr > 0x8000_0000_0000 {
        return None;
    }
    let hdr = unsafe { &*(addr as *const TableHeader) };
    if &hdr.signature != expect {
        return None;
    }
    let len = hdr.length as usize;
    if len < 36 || len > 0x100_0000 {
        return None;
    }
    // 8-bit checksum: all bytes of the table (incl. header) must sum to 0.
    let mut sum = 0u8;
    for i in 0..len {
        sum = sum.wrapping_add(unsafe { read_volatile((addr + i) as *const u8) });
    }
    if sum != 0 {
        return None;
    }
    Some((hdr.length, len))
}

// ── Parsed machine topology ───────────────────────────────────────────────────

/// What the ACPI tables tell us about the platform.  `0` = not described.
#[derive(Clone, Copy, Debug, Default)]
pub struct AcpiInfo {
    /// GICv3 distributor base (MADT GIC distributor subtable).
    pub gic_dist_base: usize,
    /// GICv3 first redistributor base (MADT GIC redistributor subtable).
    pub gic_redist_base: usize,
    /// GIC ITS base (MADT GIC ITS subtable), 0 = no ITS / LPIs.
    pub its_base: usize,
    /// NS console UART (SPCR Generic Address Structure), 0 = none.
    pub uart_base: usize,
    /// PCIe ECAM base (MCFG segment 0 entry), 0 = no PCIe.
    pub ecam_base: usize,
}

// ── RSDP → XSDT ───────────────────────────────────────────────────────────────

/// Validate an RSDP v2 structure and return the XSDT address.
fn rsdp_xsdt(rsdp: usize) -> Option<usize> {
    const SIG: &[u8; 8] = b"RSD PTR ";
    if rsdp == 0 || rsdp & 7 != 0 {
        return None;
    }
    let hdr = unsafe { &*(rsdp as *const [u8; 8]) };
    if hdr != SIG {
        return None;
    }
    let rev = unsafe { read_volatile((rsdp + 15) as *const u8) };
    if rev < 2 {
        return None; // RSDP v1 has no XSDT — we only support v2
    }
    let xsdt = unsafe { read_volatile((rsdp + 24) as *const u64) } as usize;
    if xsdt == 0 {
        return None;
    }
    // RSDP checksum (first 20 bytes sum to 0).  Optional but cheap.
    let mut sum = 0u8;
    for i in 0..20 {
        sum = sum.wrapping_add(unsafe { read_volatile((rsdp + i) as *const u8) });
    }
    if sum != 0 {
        return None;
    }
    Some(xsdt)
}

/// Walk the XSDT's table-pointer array, parsing the ones we care about.
fn parse_xsdt(xsdt: usize, out: &mut AcpiInfo) {
    let Some((_, len)) = valid_table(xsdt, b"XSDT") else { return };
    // Entries are u64 physical addresses, 8 bytes each.
    for i in 0..((len - 36) / 8) {
        let entry =
            unsafe { read_volatile((xsdt + 36 + i * 8) as *const u64) } as usize;
        if entry == 0 {
            continue;
        }
        let Some((_, tlen)) = valid_table(entry, b"    ") else { continue };
        let sig = unsafe { &*(entry as *const [u8; 4]) };
        match sig {
            b"MADT" => parse_madt(entry, tlen, out),
            b"SPCR" => parse_spcr(entry, tlen, out),
            b"MCFG" => parse_mcfg(entry, tlen, out),
            _ => {}
        }
    }
}

// ── MADT (interrupt controllers) ──────────────────────────────────────────────

fn parse_madt(addr: usize, len: usize, out: &mut AcpiInfo) {
    // Header(36) + Local Interrupt Controller Address(4) + Flags(4) = 44,
    // then a stream of type(1)/length(1) subtables.
    let mut off = 44;
    while off + 2 <= len {
        let stype = unsafe { read_volatile((addr + off) as *const u8) };
        let slen = unsafe { read_volatile((addr + off + 1) as *const u8) } as usize;
        if slen < 2 || off + slen > len {
            break;
        }
        match stype {
            // GIC Distributor: type, len, reserved(2), GIC ID(4),
            // physical base(8), system vector(4), GIC version(1)...
            0x03 if slen >= 24 => {
                let base =
                    unsafe { read_volatile((addr + off + 8) as *const u64) } as usize;
                out.gic_dist_base = base;
            }
            // GIC Redistributor: type, len, reserved(2), base(8), range(4).
            0x04 if slen >= 16 => {
                let base =
                    unsafe { read_volatile((addr + off + 4) as *const u64) } as usize;
                if out.gic_redist_base == 0 {
                    out.gic_redist_base = base;
                }
            }
            // GIC ITS: type, len, reserved(2), instance(2), base(8).
            0x0B if slen >= 16 => {
                let base =
                    unsafe { read_volatile((addr + off + 8) as *const u64) } as usize;
                if out.its_base == 0 {
                    out.its_base = base;
                }
            }
            _ => {}
        }
        off += slen;
    }
}

// ── SPCR (serial port) ────────────────────────────────────────────────────────

fn parse_spcr(addr: usize, _len: usize, out: &mut AcpiInfo) {
    // Header(36) + interface type(1) + reserved(3) + Generic Address
    // Structure(12) + interrupt type(1) + ...  The GAS at offset 40:
    // addr space id(1) + bit width(1) + bit offset(1) + access size(1) +
    // address(8).
    let gas = addr + 40;
    let space = unsafe { read_volatile(gas as *const u8) };
    if space != 0 {
        return; // only memory-mapped (MMIO) consoles
    }
    let uart = unsafe { read_volatile((gas + 4) as *const u64) } as usize;
    if uart != 0 {
        out.uart_base = uart;
    }
}

// ── MCFG (PCIe ECAM) ──────────────────────────────────────────────────────────

fn parse_mcfg(addr: usize, len: usize, out: &mut AcpiInfo) {
    // Header(36) + reserved(8) + entries of 16 bytes: base(8), seg(2),
    // bus start(1), bus end(1), reserved(4).
    let mut off = 44;
    while off + 16 <= len {
        let base = unsafe { read_volatile((addr + off) as *const u64) } as usize;
        let seg = unsafe { read_volatile((addr + off + 8) as *const u16) } as usize;
        if seg == 0 && base != 0 {
            out.ecam_base = base;
            return; // segment 0 is the only one we ever need
        }
        off += 16;
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Parse the platform topology from an RSDP address (e.g. obtained from the
/// UEFI configuration table).  Returns `None` if the tables are missing or
/// corrupt.
pub fn probe(rsdp: usize) -> Option<AcpiInfo> {
    let xsdt = rsdp_xsdt(rsdp)?;
    let mut info = AcpiInfo::default();
    parse_xsdt(xsdt, &mut info);
    // The console UART is the one thing a real boot cannot do without —
    // refuse a "successful" parse that found none of the core components.
    if info.gic_dist_base == 0 || info.uart_base == 0 {
        return None;
    }
    Some(info)
}
