//! Binary image loader.
//!
//! Phase 2 supports two image formats:
//!
//!   Flat binary — raw machine code, loaded at `ram_base`.  The entry point
//!   is the very first byte.  This is what the Zephyr RTOS stub produces for
//!   the `qemu_cortex_a53` target when built with `-DCONFIG_XIP=y`.
//!
//!   ELF64 LE AArch64 — standard ELF.  Only LOAD segments are copied.
//!   The entry point is taken from the ELF header `e_entry` field.
//!   Stripped / static binaries only (no dynamic linker).

use crate::hypervisor::HvError;
use crate::mem::PhysAddr;

// ── ELF64 constants ───────────────────────────────────────────────────────────

const ELFMAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1; // little-endian
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;

/// ELF64 header (52 bytes of interest, AArch64 LE).
#[repr(C, packed)]
struct Elf64Hdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    _e_shoff: u64,
    _e_flags: u32,
    _e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    _rest: [u8; 14],
}

/// ELF64 program header.
#[repr(C, packed)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    _p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    _p_align: u64,
}

// ── Loader entry points ───────────────────────────────────────────────────────

/// Detect the image format, load it, and return the guest entry point.
///
/// `ram_base` — physical base address of the guest RAM region.
/// `ram_size` — size in bytes of the guest RAM region.
pub fn load_flat(image: &[u8], ram_base: PhysAddr, ram_size: usize) -> Result<PhysAddr, HvError> {
    if image.len() > ram_size {
        log::error!(
            "loader: image {} B > ram {} B",
            image.len(), ram_size
        );
        return Err(HvError::NoMemory);
    }

    // Try ELF first.
    if image.len() >= 64 && image[0..4] == ELFMAG {
        load_elf64(image, ram_base, ram_size)
    } else {
        load_raw(image, ram_base)
    }
}

/// Copy a raw binary image to `ram_base`.  Entry = `ram_base`.
fn load_raw(image: &[u8], ram_base: PhysAddr) -> Result<PhysAddr, HvError> {
    let dst = ram_base as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(image.as_ptr(), dst, image.len());
    }
    log::info!(
        "loader: raw binary {} B → {:#x}, entry={:#x}",
        image.len(), ram_base, ram_base
    );
    Ok(ram_base)
}

/// Parse an ELF64 AArch64 image and copy LOAD segments into guest RAM.
fn load_elf64(image: &[u8], ram_base: PhysAddr, _ram_size: usize) -> Result<PhysAddr, HvError> {
    // SAFETY: we have already confirmed image.len() >= 64.
    let hdr: &Elf64Hdr = unsafe { &*(image.as_ptr() as *const Elf64Hdr) };

    // Validate magic / class / endianness / machine.
    if hdr.e_ident[4] != ELFCLASS64
        || hdr.e_ident[5] != ELFDATA2LSB
        || { hdr.e_machine } != EM_AARCH64
    {
        log::error!("loader: ELF header validation failed");
        return Err(HvError::NotSupported);
    }

    let phoff = { hdr.e_phoff } as usize;
    let phentsize = { hdr.e_phentsize } as usize;
    let phnum = { hdr.e_phnum } as usize;
    let entry = { hdr.e_entry } as PhysAddr;

    log::info!(
        "loader: ELF64 entry={:#x} phdrs={} @ off={:#x}",
        entry, phnum, phoff
    );

    for i in 0..phnum {
        let phdr_off = phoff + i * phentsize;
        if phdr_off + phentsize > image.len() {
            return Err(HvError::NotSupported);
        }
        let phdr: &Elf64Phdr = unsafe {
            &*(image[phdr_off..].as_ptr() as *const Elf64Phdr)
        };

        if { phdr.p_type } != PT_LOAD {
            continue;
        }

        let file_offset = { phdr.p_offset } as usize;
        let file_size = { phdr.p_filesz } as usize;
        let mem_size = { phdr.p_memsz } as usize;
        let vaddr = { phdr.p_vaddr } as usize;

        // Translate guest virtual address to host physical address.
        // For a statically linked binary the vaddr IS the physical address
        // relative to its load base.
        let dest_phys = ram_base + vaddr;
        let dst = dest_phys as *mut u8;

        // Copy file bytes.
        if file_size > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image[file_offset..].as_ptr(),
                    dst,
                    file_size,
                );
            }
        }

        // Zero BSS (mem_size - file_size).
        if mem_size > file_size {
            unsafe {
                core::ptr::write_bytes(dst.add(file_size), 0, mem_size - file_size);
            }
        }

        log::debug!(
            "loader: LOAD vaddr={:#x} file={}B mem={}B",
            vaddr, file_size, mem_size
        );
    }

    Ok(entry)
}
