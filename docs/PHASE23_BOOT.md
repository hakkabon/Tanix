# Phase 23 — Booting Tanix on Dragon Q6A

This document covers the concrete steps for the Day-3 / Day-4 UEFI boot
probe described in `PHASE23_CHECKLIST.md`.

---

## Prerequisites

| Item | Purpose |
|------|---------|
| USB-serial adapter (3.3 V, 3-pin or 4-pin) | UART console debug header |
| USB-C to USB-A cable | EDL recovery mode |
| SD card (≥1 GiB, FAT32 or FAT16) | Boot media (safer than editing the on-board UFS) |
| `edl-ng` tool + SPI firmware snapshot | Recovery path |

### UART debug header

Connect your USB-serial adapter to the Dragon Q6A UART debug header.  From
Radxa's documentation (`docs.radxa.com/en/dragon/q6a/system-config/uart-debug`):

| Pin | Signal |
|-----|--------|
| 1   | GND    |
| 2   | TX (board → host) |
| 3   | RX (host → board) |

Baud rate: **115200 n8 1** (confirmed by the `console=ttyMSM0,115200n8` kernel
cmdline in the stock boot log).

Open the console before powering on:

```sh
# macOS
screen /dev/tty.usbserial-* 115200

# Linux
minicom -D /dev/ttyUSB0 -b 115200
```

---

## Building the Tanix EFI image

```sh
# 1. Build the QCS6490 kernel (debug; no embedded servers yet)
just kernel-qcs6490

# 2. Convert to a PE/COFF EFI application + FAT16 ESP image
just kernel-qcs6490-efi
```

This produces:
- `target/tanix-qcs6490.efi` — the EFI application binary
- `target/tanix-qcs6490-esp.img` — a 16 MiB FAT16 image containing the EFI
  binary at `\EFI\BOOT\BOOTAA64.EFI` (fallback path) and a `startup.nsh`

---

## Option A — Boot from SD card (recommended for initial bring-up)

This is the safest path: it leaves the on-board UFS and the stock OS
completely untouched.

### 1. Prepare the SD card

```sh
# Partition as GPT with a single EFI System Partition (type EF00)
# macOS: replace diskN with your SD card device (diskutil list)
diskutil eraseDisk FAT32 TANIXESP GPT /dev/diskN

# Linux
gdisk /dev/sdX     # create partition 1, type EF00, ~100 MiB
mkfs.fat -F32 /dev/sdX1
```

### 2. Copy the EFI binary

```sh
# Mount the ESP and copy the EFI binary as the fallback path.
# This overwrites the default boot entry for this SD card (fine — the
# SD card is Tanix-only; the UFS stays untouched).

# macOS — ESP typically mounts at /Volumes/TANIXESP
cp target/tanix-qcs6490.efi /Volumes/TANIXESP/EFI/BOOT/BOOTAA64.EFI

# Linux
mount /dev/sdX1 /mnt
mkdir -p /mnt/EFI/BOOT
cp target/tanix-qcs6490.efi /mnt/EFI/BOOT/BOOTAA64.EFI
umount /mnt
```

### 3. Alternatively — systemd-boot entry (non-destructive, on board ESP)

If you want to add Tanix as a **second boot option** alongside the stock OS
on the board's own ESP (without touching the stock entry), use the
systemd-boot loader entry format.  This is the *preferred path* per the
checklist because it leaves the vendor OS intact.

Copy the EFI binary to the ESP first:

```sh
# Assuming the board's ESP is mounted at /boot/efi (adjust as needed)
mkdir -p /boot/efi/EFI/tanix
cp target/tanix-qcs6490.efi /boot/efi/EFI/tanix/tanix.efi
```

Then create the loader entry.  The file below is also at
`scripts/loader-entries/tanix.conf`:

```ini
# /boot/efi/loader/entries/tanix.conf
# systemd-boot entry for the Tanix Phase-23 probe kernel.
# Copy this file to the Dragon Q6A ESP at:
#   /loader/entries/tanix.conf
# and the EFI binary to:
#   /EFI/tanix/tanix.efi
#
# At the systemd-boot menu, press Down and select "Tanix Phase-23 probe".

title   Tanix Phase-23 probe (QCS6490)
linux   /EFI/tanix/tanix.efi
# No initrd or devicetree lines: Tanix is a freestanding EFI application,
# not a Linux kernel.  systemd-boot passes x0 = image handle, x1 = EFI
# system table — exactly what Tanix's _start expects.
#
# To return to the stock OS, simply select the existing entry in the boot
# menu or remove this file from /loader/entries/.
```

### 4. Select SD-card boot

Most Dragon Q6A firmware revisions allow boot media selection:
- Hold **Volume Down** at power-on to enter the systemd-boot device menu
  and select the SD card, **or**
- Edit `/loader/loader.conf` on the board's ESP to set `timeout 5` so the
  menu appears on next boot, then select the SD card entry.

---

## Option B — ESP modification (on-board UFS)

Only attempt this after the EDL recovery dry-run (Day 2 in the checklist).
The steps are the same as Option A / systemd-boot entry above, applied to
the on-board UFS ESP.

---

## What to expect on the UART console

### Success (Day-3 goal)

Any output from Tanix's own code qualifies as a success.  The minimum
expected log (before any real hardware init succeeds):

```
[INFO ] phase 23: QCS6490 kernel alive (GENI UART=0xa90000, GICv3 GICD=0x17a00000)
[INFO ] phase 23: entered Rust at EL1
[INFO ] phase 23: Gunyah hypervisor detected — running as EL1 App
```

or, if GENI SE init fails (SE not powered / wrong base address):

```
[INFO ] Tanix kernel — aarch64 init
```
(the log line from `arch::aarch64::init()` via the UART façade — if you see
this, the EFI stub ran and BSS was zeroed, which is still progress).

### Failure modes

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| No UART output at all after Tanix entry | GENI SE not accessible at `0x00A9_0000` | Check `UART_BASE` in machine.rs; compare against board DTS |
| "EFI not found" / boot loops to stock OS | EFI binary rejected (signature check?) | Check `elf2efi.py` output; look for `Secure Boot` log lines in EDK2 output |
| Exception / abort immediately | Wrong EL on entry, or MMU drop faulted | Check `phase 23: entered Rust at EL{n}` — if EL0/3, boot path is wrong |
| Gunyah NOT detected | Board booted with `enable-kvm=1` in DTB | Note: KVM mode is acceptable; means Phase 27 takes the KVM fallback path |

---

## EDL recovery (Day-2 prerequisite)

Before modifying the on-board ESP, exercise the recovery path once:

```sh
# 1. Enter EDL mode
#    Hold the EDL button while applying power (or hold Volume Down for 10s
#    after boot on some firmware revisions).
# 2. Confirm the device enumerates
lsusb | grep "05c6:9008"   # should show "Qualcomm HS-USB QDLoader 9008"
# 3. Read-only probe (does not modify anything)
edl-ng --help
edl-ng getgpt                # dump the GPT; confirms the tool chain works
```

---

## Next steps (Phase 24)

Once the Day-3 console marker appears, the Phase 23 decision document can
be written (Day 5 of the checklist).  Phase 24 bring-up hardening will:

1. Port the GICv3 init to the QCS6490 redistributor stride and CPU
   affinity map (8 CPUs, Cortex-A78 + Cortex-A55 topology).
2. Port the timer driver to the virtual timer (PPI 27) for Gunyah EL1-App
   correctness.
3. Add a `qcs6490` linker script with the correct DRAM window and image
   base (UEFI typically loads images in the low 4 GiB).
4. Implement stage-2 page tables at EL2 (prerequisite for Phase 27
   GunyahBackend — see `docs/PHASE23_CHECKLIST.md` known-limitations).
