#!/usr/bin/env python3
"""Build the Phase-20 demo FAT16 volume (2 MiB, 512 B sectors).

Geometry (consistent with `servers/libtanix-fs` and its unit tests):
  reserved = 1 sector, 1 FAT of 2 sectors, root = 64 entries (4 sectors),
  cluster = 4 sectors (2 KiB), data area starts at sector 7.

The volume holds:
  README.TXT  — multi-line text read and logged by the fs server
  VERSION.TXT — a one-line version stamp
  DATA.BIN    — 3000 raw bytes where byte i == i % 251 (chain of 3 clusters)
  VISIT.LOG   — empty file; the fs server appends a line on every boot
                (a cluster allocation + FAT flush + verify cycle)

Usage: scripts/mkfat16.py [OUT]   (default /tmp/tanix-fat.img)
"""
import struct
import sys

TOTAL_SECTORS = 4096
RESERVED = 1
FAT_SECTORS = 2
FATS = 1
ROOT_ENTRIES = 64
SPC = 4  # sectors per cluster
SECTOR = 512
ROOT_SECTORS = ROOT_ENTRIES * 32 // SECTOR
DATA_BASE = RESERVED + FATS * FAT_SECTORS + ROOT_SECTORS


def checksum(img):
    if sys.platform == "darwin":
        import hashlib
        return hashlib.md5(img).hexdigest()[:8]
    import zlib
    return hex(zlib.crc32(img))[2:]


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/tanix-fat.img"
    img = bytearray(TOTAL_SECTORS * SECTOR)
    free = bytearray(TOTAL_SECTORS * SECTOR)

    def w16(off, v):
        struct.pack_into("<H", img, off, v)

    def w32(off, v):
        struct.pack_into("<I", img, off, v)

    # ── Boot sector (FAT16 BPB) ────────────────────────────────────────────
    w16(0x0B, 512)             # bytes/sector
    img[0x0D] = SPC            # sectors/cluster
    w16(0x0E, RESERVED)        # reserved sectors
    img[0x10] = FATS           # FAT count
    w16(0x11, ROOT_ENTRIES)    # root entries
    w16(0x13, TOTAL_SECTORS)   # total sectors (16-bit)
    img[0x15] = 0xF8           # media
    w16(0x16, FAT_SECTORS)     # FAT size
    w16(0x1C, 0x0000)          # vol id
    img[0x24] = 0x0D; img[0x25] = 0x0A  # FS type string begins "FAT16   "
    fs_type = b"FAT16   "
    img[0x36:0x36 + 8] = fs_type
    img[0x1FE] = 0x55
    img[0x1FF] = 0xAA

    # ── FAT (one copy) + files ──────────────────────────────────────────────
    fat = bytearray(2 * 2048)  # 2048 entries
    next_cluster = 2

    def alloc(data: bytes) -> int:
        nonlocal next_cluster, fat
        first = next_cluster
        clusters = (len(data) + SPC * SECTOR - 1) // (SPC * SECTOR)
        for i in range(clusters):
            c = next_cluster
            next_cluster += 1
            eoc = 0xFFFF if i == clusters - 1 else c + 1
            struct.pack_into("<H", fat, c * 2, eoc)
            off = (c - 2) * SPC * SECTOR
            chunk = data[i * SPC * SECTOR:(i + 1) * SPC * SECTOR]
            img[DATA_BASE * SECTOR + off:DATA_BASE * SECTOR + off + len(chunk)] = chunk
        return first

    def root_entry(name8_3: bytes, attr: int, first: int, size: int):
        # 8.3: 11 bytes, upper-case, space-padded (spec form — the fs
        # server's short_name() compares against spaces, not NULs).
        name = bytearray(b" " * 11)
        base, _, ext = name8_3.partition(b".")
        name[:len(base)] = base.upper()
        if ext:
            name[8:8 + len(ext)] = ext[:3].upper()
        # name(11) + attr(1) + NTRes..wrtDate(14) + startCluster(2) + size(4)
        return struct.pack("<11sB14xHI", bytes(name), attr, first, size)

    README = (b"Welcome to Tanix Phase 20.\n"
              b"The filesystem server mounts this FAT16 volume\n"
              b"from a virtio-blk disk and serves it over IPC.\n"
              b"Everything here was written on the host.\n")
    VERSION = b"tanix 0.2.0 phase 20 (virtio-blk + FAT16)\n"
    DATA = bytes(i % 251 for i in range(3000))

    c_readme = alloc(README)
    c_version = alloc(VERSION)
    c_data = alloc(DATA)

    root = bytearray(ROOT_SECTORS * SECTOR)
    entries = [
        root_entry(b"README.TXT", 0x20, c_readme, len(README)),
        root_entry(b"VERSION.TXT", 0x20, c_version, len(VERSION)),
        root_entry(b"DATA.BIN", 0x20, c_data, len(DATA)),
        # VISIT.LOG exists but is empty (start cluster 0): the fs server
        # allocates + chains its first cluster on first append.
        root_entry(b"VISIT.LOG", 0x20, 0, 0),
    ]
    for i, e in enumerate(entries):
        root[i * 32:(i + 1) * 32] = e

    img[RESERVED * SECTOR: (RESERVED + FAT_SECTORS) * SECTOR] = fat[:FAT_SECTORS * SECTOR]
    img[3 * SECTOR: (3 + ROOT_SECTORS) * SECTOR] = root

    with open(out, "wb") as f:
        f.write(img)
    print(f"FAT16 demo volume: {out} ({len(img)} bytes, md5 {checksum(bytes(img))})")


if __name__ == "__main__":
    main()