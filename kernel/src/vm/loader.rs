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

// ── ELF64 section-header constants (needed to find relocation tables) ────────

const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_NOBITS: u32 = 8;

const SHF_ALLOC: u64 = 0x2;

/// R_AARCH64_ABS64: the 8-byte word holds an absolute link-time address;
/// the loader must add the load-base delta (this image links at VMA 0).
const R_AARCH64_ABS64: u32 = 257;
/// R_AARCH64_RELATIVE: the word holds S + A; add the load-base delta.
const R_AARCH64_RELATIVE: u32 = 1027;

/// ELF64 section header.
#[repr(C, packed)]
struct Elf64Shdr {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

/// ELF64 symbol table entry.
#[repr(C, packed)]
struct Elf64Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}

/// ELF64 rela relocation entry (addend is explicit; the linked word already
/// contains the resolved S+A for ABS64, so only the base delta is applied).
#[repr(C, packed)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

/// ELF64 header (52 bytes of interest, AArch64 LE).
#[repr(C, packed)]
struct Elf64Hdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    _e_flags: u32,
    _e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
    _rest: [u8; 12],
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

/// Loaded image: entry point plus the end of the LOAD footprint
/// (highest `vaddr + memsz`, page-rounded) — Phase 6 uses the latter to
/// decide which pages of a server region must be made EL0-visible.
#[derive(Debug, Clone, Copy)]
pub struct LoadedImage {
    pub entry: PhysAddr,
    /// First byte after the loaded image (text/data/bss), 4 KiB-aligned.
    pub image_end: PhysAddr,
}

/// Detect the image format, load it, and return the guest entry point.
///
/// `ram_base` — physical base address of the guest RAM region.
/// `ram_size` — size in bytes of the guest RAM region.
pub fn load_flat(image: &[u8], ram_base: PhysAddr, ram_size: usize) -> Result<PhysAddr, HvError> {
    load_flat_full(image, ram_base, ram_size).map(|i| i.entry)
}

/// Like `load_flat` but also returns the image's LOAD footprint end.
pub fn load_flat_full(
    image: &[u8],
    ram_base: PhysAddr,
    ram_size: usize,
) -> Result<LoadedImage, HvError> {
    // Try ELF first: an ELF's file size includes non-loadable sections
    // (debug info, symbol tables), so the RAM check is done against the
    // LOAD-segment footprint inside load_elf64, not the file size.
    if image.len() >= 64 && image[0..4] == ELFMAG {
        let (entry, footprint) = load_elf64(image, ram_base, ram_size)?;
        let image_end = (footprint + 0xFFF) & !0xFFF;
        Ok(LoadedImage { entry, image_end })
    } else {
        if image.len() > ram_size {
            log::error!(
                "loader: image {} B > ram {} B",
                image.len(), ram_size
            );
            return Err(HvError::NoMemory);
        }
        load_raw(image, ram_base)?;
        let image_end = (ram_base + image.len() + 0xFFF) & !0xFFF;
        Ok(LoadedImage { entry: ram_base, image_end })
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
///
/// Returns `(entry, footprint)` where `footprint` is the highest
/// `vaddr + memsz` across LOAD segments (already validated to fit).
fn load_elf64(
    image: &[u8],
    ram_base: PhysAddr,
    ram_size: usize,
) -> Result<(PhysAddr, usize), HvError> {
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

    // The guest image links at address 0 (see servers/link.ld and
    // servers/zephyr-stub/link.ld), so e_entry is an *offset* within the
    // image.  The absolute guest entry is the image's load base plus that
    // offset.
    let entry = ram_base + { hdr.e_entry } as usize;

    log::info!(
        "loader: ELF64 entry={:#x} phdrs={} @ off={:#x}",
        entry, phnum, phoff
    );

    // First pass: validate the program headers and compute the LOAD
    // footprint (highest vaddr + memsz) to check it fits in the RAM region.
    let mut footprint: usize = 0;
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

        let end = { phdr.p_vaddr } as usize + { phdr.p_memsz } as usize;
        if end > footprint {
            footprint = end;
        }
    }

    if footprint > ram_size {
        log::error!(
            "loader: ELF footprint {} B > ram {} B",
            footprint, ram_size
        );
        return Err(HvError::NoMemory);
    }

    // Second pass: copy the LOAD segments.
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

    // The image links at VMA 0 and the code is position-independent
    // (adrp/ldr), so it runs correctly at any load base — but absolute
    // pointers embedded in .data/.rodata (e.g. core's panic-message format
    // pieces and Location file pointers) were resolved by the linker to
    // *link-time* addresses.  Apply the load-base delta to those words now,
    // mirroring how a dynamic loader processes R_AARCH64_ABS64/RELATIVE.
    apply_relocations(image, ram_base, footprint);

    Ok((entry, footprint))
}

/// Apply base-0 relocations: for every relocation record targeting an
/// allocated non-text section, add `ram_base` to the referenced 8-byte word.
/// The word already holds the resolved link-time value (S + A for ABS64), so
/// only the `ram_base` delta is needed.
fn apply_relocations(image: &[u8], ram_base: PhysAddr, footprint: usize) {
    let hdr = unsafe { &*(image.as_ptr() as *const Elf64Hdr) };

    let shoff = { hdr.e_shoff } as usize;
    let shentsize = { hdr.e_shentsize } as usize;
    let shnum = { hdr.e_shnum } as usize;
    let shstrndx = { hdr.e_shstrndx } as usize;

    if shoff == 0 || shstrndx == 0 || shnum == 0 || shentsize < 64 {
        return; // no section headers (stripped) → nothing to relocate
    }

    let shstr_off = {
        let s = shoff + shstrndx * shentsize;
        if s + 64 > image.len() {
            return;
        }
        let shdr = unsafe { &*(image[s..].as_ptr() as *const Elf64Shdr) };
        let off = shdr.sh_offset;
        off as usize
    };
    if shstr_off >= image.len() {
        return;
    }

    let mut applied = 0;
    for i in 0..shnum {
        let s = shoff + i * shentsize;
        if s + 64 > image.len() {
            continue;
        }
let shdr = unsafe { &*(image[s..].as_ptr() as *const Elf64Shdr) };
        // Skip relocation sections whose target is not an allocated,
        // writable/read-only DATA section (text relocations are already
        // resolved and position-independent; debug relocations are not in
        // the loaded image).
        let info_idx = { shdr.sh_info } as usize;
        if info_idx >= shnum {
            continue;
        }
        let info = info_idx * shentsize;
        let target = unsafe { &*(image[shoff + info..].as_ptr() as *const Elf64Shdr) };
        if ({ target.sh_flags } & SHF_ALLOC) == 0
            || { target.sh_addr } == 0
            || { target.sh_addr } >= footprint as u64
        {
            continue;
        }

        let rel_off = { shdr.sh_offset } as usize;
        let rel_size = { shdr.sh_size } as usize;
        let entsize = { shdr.sh_entsize } as usize;
        if entsize == 0 || rel_off + rel_size > image.len() {
            continue;
        }

        let count = rel_size / entsize;
        for k in 0..count {
            let r = unsafe { &*(image[rel_off + k * entsize..].as_ptr() as *const Elf64Rela) };
            let r_type = ({ r.r_info } & 0xffff_ffff) as u32;
            if r_type != R_AARCH64_ABS64 && r_type != R_AARCH64_RELATIVE {
                continue;
            }
            // r_offset is a guest VMA (link-time address in the image);
            // the word lives at ram_base + r_offset already (segments were
            // copied there) and holds the resolved link-time value.
            let dst = (ram_base + { r.r_offset } as usize) as *mut u64;
            let val = unsafe { core::ptr::read_volatile(dst) };
            let corrected = val.wrapping_add(ram_base as u64);
            unsafe { core::ptr::write_volatile(dst, corrected) };
            applied += 1;
        }
    }
    if applied > 0 {
        log::info!("loader: applied {} base-0 relocations", applied);
    }
}
