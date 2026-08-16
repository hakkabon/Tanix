#!/usr/bin/env python3
"""Phase 18: build a UEFI-bootable image of the Tanix kernel.

Converts the linker-produced ELF (linked at its physical RAM address) into a
PE32+ AArch64 EFI application, then wraps it into a FAT16 ESP as
\EFI\BOOT\BOOTAA64.EFI so edk2 can boot it straight from removable media.

The PE image is *not* relocatable by design: the kernel is linked at a fixed
physical address (0x40080000 on virt, 0x10000080000 on sbsa-ref) and edk2
loads it at exactly that ImageBase.  If edk2 cannot allocate those pages the
LoadImage fails visibly and the firmware reports it on the serial console.

Usage:
    elf2efi.py kernel.elf [esp.img]
"""

import struct
import sys

SECT_ALIGN = 0x1000
FILE_ALIGN = 0x200

IMAGE_SUBSYSTEM_EFI_APPLICATION = 10
IMAGE_FILE_MACHINE_ARM64 = 0xAA64
IMAGE_FILE_EXECUTABLE_IMAGE = 0x0002
IMAGE_FILE_LARGE_ADDRESS_AWARE = 0x0020
IMAGE_SCN_CNT_CODE = 0x00000020
IMAGE_SCN_CNT_INITIALIZED_DATA = 0x00000040
IMAGE_SCN_CNT_UNINITIALIZED_DATA = 0x00000080
IMAGE_SCN_MEM_EXECUTE = 0x20000000
IMAGE_SCN_MEM_READ = 0x40000000
IMAGE_SCN_MEM_WRITE = 0x80000000


def align_up(v, a):
    return (v + a - 1) & ~(a - 1)


def parse_elf(path):
    data = open(path, "rb").read()
    assert data[:4] == b"\x7fELF" and data[4] == 2, "not an ELF64"
    assert data[5] == 1, "not little-endian"
    e_phoff = struct.unpack_from("<Q", data, 0x20)[0]
    e_phentsize = struct.unpack_from("<H", data, 0x36)[0]
    e_phnum = struct.unpack_from("<H", data, 0x38)[0]
    e_entry = struct.unpack_from("<Q", data, 0x18)[0]

    segs = []
    imagebase = None
    for i in range(e_phnum):
        ph = e_phoff + i * e_phentsize
        p_type = struct.unpack_from("<I", data, ph)[0]
        if p_type != 1:  # PT_LOAD
            continue
        flags = struct.unpack_from("<I", data, ph + 4)[0]
        off = struct.unpack_from("<Q", data, ph + 8)[0]
        vaddr = struct.unpack_from("<Q", data, ph + 0x10)[0]
        filesz = struct.unpack_from("<Q", data, ph + 0x20)[0]
        memsz = struct.unpack_from("<Q", data, ph + 0x28)[0]
        if imagebase is None or vaddr < imagebase:
            imagebase = vaddr
        if off == 0 and filesz <= 0x400:
            continue  # lld's ELF-header-only PT_LOAD; not part of the image
        segs.append((flags, off, vaddr, filesz, memsz))
    assert imagebase % SECT_ALIGN == 0, f"imagebase {imagebase:#x} not aligned"
    # edk2 rejects images whose sections start below SizeOfHeaders, so the
    # first PT_LOAD must be at RVA 0x1000.  The kernel is linked so that its
    # lowest load segment is where the code must run (e.g. 0x10000080000);
    # use the page just below it as the PE ImageBase — the header page.
    imagebase -= SECT_ALIGN
    return data, e_entry, imagebase, segs


def pe_sections(segs, imagebase):
    """Map ELF PT_LOAD segments to PE sections (RVA = vaddr - imagebase).

    The debug linker script aligns output sections to 8 bytes, so segment
    *starts* need not be page-aligned — but PE section RVAs must be.  We
    keep the exact vaddr for every section (symbols are absolute) and
    extend each section's VirtualSize to the *next* section's start; the
    PE loader zero-fills the tail, so the whole contiguous image range is
    backed by memory even though the on-disk raw data is only that of the
    real segment.
    """
    assert all(vaddr >= imagebase for _, _, vaddr, _, _ in segs), "non-contiguous base"
    sections = []
    used_names = set()

    def name_for(flags, filesz, base):
        if flags & 2:  # PF_W
            name = ".bss" if filesz == 0 else ".data"
        elif flags & 1:  # PF_X
            name = ".text"
        else:
            name = ".rodata"
        n = name
        i = 1
        while n in used_names:
            n = f"{name}{i}"
            i += 1
        used_names.add(n)
        return n

    for flags, off, vaddr, filesz, memsz in segs:
        rva = vaddr - imagebase
        assert rva % SECT_ALIGN == 0, f"section {vaddr:#x} not section-aligned"
        name = name_for(flags, filesz, vaddr)
        chars = IMAGE_SCN_MEM_READ
        if flags & 2:  # PF_W
            chars |= IMAGE_SCN_MEM_WRITE
        if flags & 1:  # PF_X
            chars |= IMAGE_SCN_MEM_EXECUTE | IMAGE_SCN_CNT_CODE
        if filesz == 0:
            chars |= IMAGE_SCN_CNT_UNINITIALIZED_DATA
        else:
            chars |= IMAGE_SCN_CNT_INITIALIZED_DATA
        sections.append([name, rva, memsz, filesz, off, chars])

    # Extend each section to cover the gap before the next one.
    sections.sort(key=lambda s: s[1])
    for i in range(len(sections) - 1):
        gap_end = sections[i + 1][1]
        if gap_end > sections[i][1] + sections[i][2]:
            sections[i][2] = gap_end - sections[i][1]
    return sections


def write_pe(path, data, e_entry, segs, imagebase):
    sections = pe_sections(segs, imagebase)
    assert sections, "no PT_LOAD segments"
    assert all(vaddr >= imagebase for _, _, vaddr, _, _ in segs)

    entry_rva = e_entry - imagebase
    code = [s for s in sections if s[5] & IMAGE_SCN_CNT_CODE]
    init_data = [s for s in sections if s[5] & IMAGE_SCN_CNT_INITIALIZED_DATA]
    uninit_data = [s for s in sections if s[5] & IMAGE_SCN_CNT_UNINITIALIZED_DATA]

    pe = bytearray()
    pe += b"MZ" + b"\x00" * 0x3A + struct.pack("<I", 0x80)
    pe += b"\x00" * (0x80 - len(pe))  # pad DOS header to the PE signature
    pe += b"PE\x00\x00"
    # COFF header
    pe += struct.pack(
        "<HHIIIHH",
        IMAGE_FILE_MACHINE_ARM64,
        len(sections),
        0,  # timestamp
        0,  # symbol table
        0,  # symbols
        0xF0,  # optional header size (PE32+)
        IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_LARGE_ADDRESS_AWARE,
    )
    # PE32+ optional header
    pe += struct.pack("<H", 0x20B)  # magic
    pe += struct.pack("<B", 0)  # linker major
    pe += struct.pack("<B", 0)  # linker minor
    pe += struct.pack(
        "<IIIII",
        sum(s[3] for s in code),
        sum(s[3] for s in init_data),
        sum(s[4] for s in uninit_data),
        entry_rva,
        sections[0][1],
    )
    pe += struct.pack("<Q", imagebase)  # ImageBase
    pe += struct.pack("<II", SECT_ALIGN, FILE_ALIGN)
    pe += struct.pack("<HHHHHH", 0, 0, 0, 0, 0, 0)  # os/image/subsystem versions
    pe += struct.pack("<I", 0)  # Win32VersionValue
    size_of_image = align_up(max(s[1] + s[2] for s in sections), SECT_ALIGN)
    # SizeOfHeaders must cover DOS+PE headers and *all* section headers
    # (edk2 GetImageInfo rejects images where it doesn't).
    size_of_headers = align_up(0x80 + 0x18 + 0xF0 + len(sections) * 40, FILE_ALIGN)
    pe += struct.pack("<II", size_of_image, size_of_headers)  # SizeOfImage, SizeOfHeaders
    pe += struct.pack("<I", 0)  # CheckSum
    pe += struct.pack("<HH", IMAGE_SUBSYSTEM_EFI_APPLICATION, 0)  # subsystem, dllchars
    pe += struct.pack("<QQQQ", 0x4000, 0x1000, 0x1000, 0x1000)  # stack/commit, heap/commit
    pe += struct.pack("<II", 0, 16)  # loader flags, NumberOfRvaAndSizes
    pe += struct.pack("<Q", 0) * 16  # data directories (none)
    assert len(pe) == 0x80 + 0x18 + 0xF0

    # Section headers + raw data
    cur = align_up(len(pe) + len(sections) * 40, FILE_ALIGN)
    raws = [data[off : off + filesz] if filesz else b"" for _, _, _, filesz, off, _ in sections]
    raw_offs = []
    for raw in raws:
        raw_offs.append(cur if raw else 0)
        if raw:
            cur += align_up(len(raw), FILE_ALIGN)
    out = bytearray(pe)
    for (name, rva, memsz, filesz, off, chars), raw, roff in zip(sections, raws, raw_offs):
        out += struct.pack(
            "<8sIIIIIIHHI",
            name.encode()[:8].ljust(8, b"\x00"),
            memsz,
            rva,
            align_up(len(raw), FILE_ALIGN) if raw else 0,
            roff,
            0,  # relocs
            0,  # line numbers
            0,
            0,
            chars,
        )
    for raw, roff in zip(raws, raw_offs):
        if not raw:
            continue
        out += b"\x00" * (roff - len(out))
        out += raw
        out += b"\x00" * (align_up(len(raw), FILE_ALIGN) - len(raw))
    open(path, "wb").write(out)
    print(f"PE/COFF: {path} ({len(out)} bytes, imagebase={imagebase:#x}, "
          f"entry rva={entry_rva:#x}, {len(sections)} sections)")
    return out, imagebase


def write_fat16(path, files):
    """Create a FAT16 image with the given {path: data} files."""
    sectors = 32768  # 16 MiB @ 512 B
    bps = 512
    spc = 1  # 1 sector per cluster -> 512 B clusters, 32676 data clusters
    rde_count = 512
    rde_sectors = rde_count * 32 // bps
    fats = 2
    reserved = 1
    fat_sectors = 128  # 64 KiB each -> 32768 entries
    data_start = reserved + fats * fat_sectors + rde_sectors
    data_clusters = sectors - data_start
    assert data_clusters < 0xFFF0, "too many clusters for FAT16"
    assert data_clusters * spc >= 16 * 1024 * 1024 // bps - data_start or True

    img = bytearray(sectors * bps)
    struct.pack_into("<BBB", img, 0, 0xEB, 0x3C, 0x90)  # jmp
    img[3:11] = b"TANIX   "
    struct.pack_into("<H", img, 11, bps)
    img[13] = spc
    struct.pack_into("<H", img, 14, reserved)
    img[16] = fats
    struct.pack_into("<H", img, 17, rde_count)
    struct.pack_into("<H", img, 19, data_clusters)
    img[21] = 0xF8
    struct.pack_into("<H", img, 22, fat_sectors)
    struct.pack_into("<H", img, 24, 1)  # sectors/track
    struct.pack_into("<H", img, 26, 1)  # heads
    struct.pack_into("<I", img, 28, 0)  # hidden
    struct.pack_into("<I", img, 32, sectors)
    struct.pack_into("<H", img, 38, 0x29)  # extended boot sig
    struct.pack_into("<I", img, 39, 0x54584154)  # volume id "TAXT"
    img[43:54] = b"TANIX ESP  "
    img[54:62] = b"FAT16   "

    # FATs: cluster 0 = 0xFFF8 (media), cluster 1 = 0xFFFF (reserved)
    chain = {}  # dir path -> list of clusters
    fat = [0xFFF8, 0xFFFF]
    for fpath, content in files.items():
        parts = fpath.upper().split("/")[1:]
        clusters = []
        for off in range(0, len(content), bps * spc):
            clusters.append(len(fat))
            fat.append(0xFFFF)
        for i, c in enumerate(clusters[:-1]):
            fat[c] = clusters[i + 1]
        dirs = "/".join(parts)
        chain[dirs] = clusters

    def dir_entry(name8, ext3, attr, first_cluster, size, seq_no=None):
        e = bytearray(32)
        if seq_no is not None:
            e[0] = seq_no  # 0x40 | n for long names — not used, plain 8.3 only
        e[0:8] = name8.ljust(8).encode()
        e[8:11] = ext3.ljust(3).encode()
        e[11] = attr
        # cluster_high@20, time@22, date@24, cluster_low@26, size@28
        struct.pack_into("<HHHH", e, 20, first_cluster >> 16, 0, 0, first_cluster & 0xFFFF)
        struct.pack_into("<I", e, 28, size)
        return bytes(e)

    # Directory layout: a root directory entry for every file (subdirectories
    # are skipped — all files here live at the FAT root or in /EFI/BOOT,
    # which we special-case with one subdirectory cluster).
    root_off = (reserved + fats * fat_sectors) * bps
    # EFI directory: needs a cluster to hold the BOOT entry.
    efi_dir_cluster = len(fat)
    fat.append(0xFFFF)
    efi_dir_off = (data_start + efi_dir_cluster - 2) * bps
    img[root_off : root_off + 32] = dir_entry("EFI", "   ", 0x10, efi_dir_cluster, 0)
    root_off += 32
    boot_dir_cluster = len(fat)
    fat.append(0xFFFF)
    boot_dir_off = (data_start + boot_dir_cluster - 2) * bps
    img[efi_dir_off : efi_dir_off + 32] = dir_entry("BOOT", "   ", 0x10, boot_dir_cluster, 0)

    # Map FAT path -> (dir_offset, name, ext) for the entries.
    def write_file(fpath, content, dir_off):
        parts = fpath.upper().split("/")[1:]
        name, _, ext = parts[-1].partition(".")
        fclusters = chain["/".join(parts)]
        img[dir_off : dir_off + 32] = dir_entry(
            name, ext, 0x20, fclusters[0], len(content)
        )
        for c in fclusters:
            off = (data_start + c - 2) * bps
            img[off : off + bps * spc] = (content[: bps * spc] + b"\x00" * (bps * spc))[: bps * spc]
            content = content[bps * spc :]

    for fpath, content in files.items():
        if fpath == "/startup.nsh":
            write_file(fpath, content, root_off)
            root_off += 32
        elif fpath == "/EFI/BOOT/BOOTAA64.EFI":
            write_file(fpath, content, boot_dir_off)

    # Write FATs (both copies)
    for fat_copy in range(fats):
        fat_off = (reserved + fat_copy * fat_sectors) * bps
        flat = struct.pack("<%dH" % len(fat), *fat)
        img[fat_off : fat_off + len(flat)] = flat
    img[510:512] = b"\x55\xAA"
    open(path, "wb").write(img)
    print(f"FAT16 ESP: {path} ({len(img)} bytes)")


def main():
    elf, out = sys.argv[1], sys.argv[2]
    data, entry, imagebase, segs = parse_elf(elf)
    write_pe(out, data, entry, segs, imagebase)
    write_fat16(
        sys.argv[3] if len(sys.argv) > 3 else "/tmp/esp.img",
        {
            "/EFI/BOOT/BOOTAA64.EFI": open(out, "rb").read(),
            # Auto-run from the UEFI shell's first file system.
            "/startup.nsh": b"\\EFI\\BOOT\\BOOTAA64.EFI\n",
        },
    )


if __name__ == "__main__":
    main()
