# Phase 23 — Hardware spike: week-one checklist

**Goal:** answer, with evidence, "does Tanix boot at all on Dragon Q6A —
bare-metal via UEFI, or as a Gunyah guest?" before committing to either
story for Phase 24 onward. One board (Dragon Q6A), throwaway code
welcome, decision-document is the deliverable.

## What we already know (desk research, not yet hardware-verified)

Before touching a board, here's what's publicly documented about Dragon
Q6A's boot chain — this de-risks the spike considerably and changes it
from "explore blind" to "verify a specific hypothesis":

- **Boot chain** (from a community board-bring-up writeup): `PBL (ROM) →
  XBL → TZ/HYP → UEFI/EDK2`, then UEFI scans for an ESP (GPT-labeled FAT
  partition) and boots a standard EFI bootloader from it — this is the
  same shape as our Phase 18 `sbsa-ref` EFI/ACPI path, not a
  from-scratch bring-up.
- **Gunyah is active by default, out of the box.** A first-hand serial
  boot log from this exact board shows, in order:
  ```
  enable-kvm is not set in DTB, booting with EL1 App
  ...
  Gunyah based bootup
  Exit EBS        [31655] UEFI End
  ```
  i.e. stock Linux is *already* launched as an EL1 App under Gunyah on
  this board's default firmware — Gunyah isn't an optional add-on here,
  it's the standing configuration. The `enable-kvm` DTB property looks
  like the toggle between this and some other launch mode (possibly
  direct EL2/KVM) and is priority one to understand on-device.
- **The ESP layout is systemd-boot-standard**, not proprietary: an
  `EFI/BOOT/BOOTAA64.EFI` → `EFI/systemd/systemd-bootaa64.efi` chain,
  `/loader/loader.conf` + `/loader/entries/*.conf` boot entries, each
  entry a plain `title` / `linux` / `initrd` / `devicetree` block
  pointing at files on the ESP. This means adding a **second, additional
  boot entry** for a Tanix EFI image is plausible without touching the
  vendor OS at all — no evidence so far of a locked-down or
  signature-checked boot path (the same firmware happily boots
  community Armbian images).
- **Console is `ttyMSM0`**, i.e. Qualcomm's own UART IP, not a PL011 —
  confirms the `sbsa-ref` PL011 driver needs a real port, not a config
  tweak, in Phase 24.
- **`irqchip.gicv3_pseudo_nmi=0`** appears in the default kernel cmdline,
  confirming GICv3 with pseudo-NMI support present but disabled by
  default.
- **Flashing/recovery is well-documented and vendor-supported**: EDL
  mode (hold the onboard EDL button while powering on, or short-press
  after boot; device enumerates as USB `05c6:9008`) + Radxa's `edl-ng`
  tool can always reflash the SPI-NOR boot firmware from a host PC,
  which means a bad experiment is recoverable, not a bricked board.

None of this is hardware-verified yet — it's secondhand, from one
community post and one upstream patch series, both of which could be
stale, board-revision-specific, or simply wrong. Every item above is a
hypothesis to confirm in week one, not a fact to build on unchecked.

## Prerequisites (order to acquire/prepare, before the board arrives if possible)

- [ ] Order the board (Dragon Q6A) + a USB-C-to-USB-A cable for EDL +
      a 3-pin (or documented pinout) USB-serial adapter for the UART
      debug header — check `docs.radxa.com/en/dragon/q6a/system-config/uart-debug`
      for the exact pin-out and baud (the boot log above suggests
      115200n8, matching `console=ttyMSM0,115200n8`).
- [ ] Download and read, before power-on: Radxa's "Getting Started",
      "Low-level Development" (BIOS/EDL/SPI-firmware pages), and the
      board schematic from Radxa's Resource Downloads page.
- [ ] Download `edl-ng` (Radxa's flashing tool) and the latest SPI boot
      firmware snapshot, so a recovery path is ready *before* the first
      custom-image experiment, not scrambled together after a bricked
      boot.
- [ ] Pull `arch/arm64/boot/dts/qcom/qcs6490-radxa-dragon-q6a.dts` from
      the upstream kernel patch series (Xilin Wu, `dt-bindings: arm:
      qcom: Add Radxa Dragon Q6A` / `arm64: dts: qcom: qcs6490:
      Introduce Radxa Dragon Q6A`) — this gives real GICv3, UART, and
      PCIe ECAM register addresses to compare against `sbsa-ref`'s
      before writing a single line of `machine.rs` code in Phase 24.
- [ ] Have a spare SD card or USB drive on hand — the community writeup
      above used the ESP on `/dev/sdb2`, i.e. removable-media boot is
      viable and much safer to experiment on than the onboard UFS/eMMC.

## Day-by-day plan

**Day 1 — passive observation.** Boot the board exactly as it ships
(don't flash anything). Capture the full UART log from cold boot
through to a shell prompt. Confirm or refute, in writing:
- Does the log show `Gunyah based bootup` as documented above?
- What exactly does `enable-kvm is not set in DTB` mean here — check
  the shipped `qcs6490-radxa-dragon-q6a.dtb` (`dtc -I dtb -O dts`) for
  an `enable-kvm` property and note its location/siblings in the tree.
- Confirm the ESP layout (`EFI/BOOT/BOOTAA64.EFI`,
  `EFI/systemd/systemd-bootaa64.efi`, `/loader/loader.conf`,
  `/loader/entries/*.conf`) matches the community report on this
  board's current firmware revision.

**Day 2 — EDL/recovery dry run.** Before modifying anything on the ESP,
deliberately exercise the recovery path once: enter EDL mode, confirm
`lsusb` shows `05c6:9008`, run `edl-ng --version` and a read-only
`edl-ng` command (e.g. reading back a partition) to prove the tool
chain works. This is the safety net for every later day — don't skip
it.

**Day 3 — bare-metal UEFI entry point.** Reuse the Phase 18 EFI stub
(`arch/aarch64/efi.rs`) to build a Tanix `.efi` image the same way it's
built for `sbsa-ref` today. Copy it onto a spare SD card's ESP as
`EFI/BOOT/BOOTAA64.EFI` or as an *additional* `systemd-boot` entry
(preferred — leaves the vendor path untouched) pointing `linux` at the
Tanix EFI binary with no `initrd`/`devicetree` override initially.
Attempt boot from the SD card (most boards let you select boot media
via a key combo or the `embloader`/systemd-boot menu seen in the log
above). Success criterion: **anything** appears on UART from Tanix code
— even a single "hello EL1" print is the whole point of this day.

**Day 4 — triage whichever way Day 3 went.**
- If Tanix's EFI stub never gets control: is UEFI rejecting the image
  (signature check — check for any secure-boot/verified-boot log lines)
  or is it a more mundane EFI-image-format problem (check the Phase 18
  `elf2efi.py` conversion against this firmware's expectations, e.g.
  machine type, subsystem field)? This determines whether the blocker
  is "signing" (hard, changes the whole roadmap) or "tooling" (easy,
  keep debugging).
- If Tanix's EFI stub does get control: is it running as a Gunyah EL1
  App (check whether the `Gunyah based bootup` log line still appears
  ahead of Tanix's own output) or did it land at EL2/EL1 the way it
  does on `sbsa-ref`? Either answer is useful — the former confirms the
  Phase 27 real-Gunyah path is at minimum reachable as *a* guest; the
  latter means UEFI dropped straight to a bare-metal-style EL1 world for
  this boot entry, sidestepping Gunyah entirely.

**Day 5 — write the decision document.** One page, answering:
1. Does Tanix's Phase 18 EFI path boot on Dragon Q6A at all (yes/no,
   with the exact failure point if no)?
2. Is the board's stock firmware definitely running production code
   under Gunyah as EL1 Apps (yes/no, with the UART evidence)?
3. Is there a *visible* mechanism (DTB property, UEFI variable, boot
   entry) to launch an *additional*, second VM under the same Gunyah
   instance — as opposed to just replacing the single primary EL1 App —
   or does that require touching the Resource Manager VM's config,
   which is very likely closed/signed? (This is probably a "no, not
   without vendor cooperation" answer going in, but it needs to be
   checked rather than assumed, since it's the crux of Phase 27.)
4. Recommendation: proceed toward Phase 24 (bare-metal hardening) only,
   or is there enough evidence to also start a parallel Phase 27 spike
   into the real `gunyah_hypercall` ABI as an EL1-App guest?

## Exit criteria

This phase is done — not perfectly, just done enough to unblock Phase 24
— when the decision document above exists and is confident enough to
commit engineering weeks against. It does **not** require a working
demo; "we now know precisely why it doesn't boot yet, and here's the
next concrete step" is an acceptable Phase 23 outcome and is much more
useful than silently retrying for a week.

## What this checklist deliberately does not cover

- CAN-FD, the real GPU/display path, and the Arduino UNO Q are all out
  of scope for Phase 23 — they depend on Phase 23's outcome and are
  separately scoped in Phases 25/26/28.
- RUBIK Pi 3 bring-up is explicitly deferred until Dragon Q6A's spike is
  done — same SoC family, so most findings here should transfer, but
  that's an assumption to re-verify, not a guarantee.
