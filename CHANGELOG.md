# Changelog

Tanix doesn't tag releases yet — this changelog is organized by the
phase roadmap in `README.md` instead, and links each phase to the
commit(s) that landed it. It is a retrospective reconstruction from
`git log`, written as part of Phase 22; entries before that point were
not tracked contemporaneously, so dates/groupings are best-effort.

## [Unreleased] — Phase 22: CI harness, docs, syscall coverage

- Added `.github/workflows/ci.yml`: lint (clippy, whole workspace) +
  five QEMU boot-regression jobs (virt/phase11 server set, virt SMP
  bring-up, virt network e2e via `net-test.sh`, sbsa-ref EL3/TrustZone
  boot, sbsa-ref Phase 21 co-tenant RTOS demo).
- Added `scripts/boot-test.sh`: a generic "boot in QEMU, assert on
  serial console markers, fail loud on timeout" primitive shared by CI
  and local use, for both `virt` and `sbsa-ref`.
- Added `docs/SYSCALLS.md`: a syscall coverage matrix (26 of 27 syscalls
  have at least one real in-tree caller) with an explicit "known gaps"
  section, plus `just syscall-coverage` to regenerate the raw data.
- Added this changelog.
- Fixed the RTOS-guest crash where tenants panicked right after their
  first `put` and the guest's own panic handler faulted while printing
  the panic location (`ef13c13`, `a794802`).

## Phase 21 — Co-tenant vCPU scheduler + Zephyr-style RTOS guest

`e11d5b4`, `0f4a22e`, `a794802`, `ef13c13`

- `vm/sched.rs`: N-way round-robin over a tenant table; the EL1 tick
  preempts a tenant mid-guest (full vCPU frame capture) and resumes it
  later exactly where it stopped.
- `servers/rtos-guest`: a Zephyr-modelled RTOS guest with
  `k_thread`/`k_sem`/`k_msgq`/`k_sleep`/`k_timer` primitives and its own
  cooperative scheduler inside each time slice.
- Two post-landing bugfixes: a guest panic-inside-panic-handler crash
  right after the first `k_msgq` `put`.

## Phase 20 — Filesystem + full TCP; automotive groundwork

`0a98b00`, `da0eb3f`, `58b713b`

- `servers/fs` + `libtanix-fs`: FAT16 over virtio-blk, served over IPC
  (open/read/write/close, listing, volume info), with a boot-time
  self-test against a demo volume.
- Full TCP (vs. the earlier UDP-first socket layer).
- A hard-won log-visibility bug: `MAX_LOG_LEVEL_FILTER` was getting
  clobbered to `Off` mid-boot, silently no-oping every `log::` call
  while a direct-MMIO EL3 hook kept printing — root-caused and fixed.

## Phase 19 — Demand paging, COW, capability-gated MMIO

`88a5792`, `2f680d0`

- `mem/vm_fault.rs`: demand-paging windows (zero-page alias until
  written), copy-on-write frame splitting on the first write fault, and
  lazy stack growth.
- `SYS_MAP_DEMAND` / `SYS_MAP_COW` / `SYS_MAP_CAP` syscalls.

## Phase 18 — UEFI/ACPI boot path

`2be90a7`, `89e7263`, `3a4fb48`

- EFI handoff (`arch/aarch64/efi.rs`), ACPI table parsing (RSDP → XSDT →
  MADT/SPCR/MCFG), and the first successful `sbsa-ref` UEFI boot through
  MMU enable (`TCR=0x24b5103510`, IPS=44 from PARANGE).
- `target-sbsa/` established as its own Cargo target directory so `virt`
  and `sbsa-ref` binaries never collide.

## Phase 17 — Secure services over SMC; attestation

`5d4bcea`, `acf5a45`

- EL3-monitor-backed secure storage, keybox (key generation/seal/unseal
  that never leaves EL3), and attestation, exposed as syscalls 18–23.
- `servers/sec`: demo server exercising the full round-trip and logging
  the sealed-vs-plaintext byte comparison.
- Verified booting on both `virt` and `sbsa-ref` (the syscalls degrade
  to `-1` on `virt`, where no EL3 monitor exists).

## Phase 16 — `sbsa-ref` boot: EL3 monitor, TrustZone

`e467b77`

- `arch/aarch64/monitor.rs` + `monitor_entry.s` + `sec_payload.s`: EL3
  reset handling, PSCI supply (`sbsa-ref` has none of its own), and a
  secure payload running at S-EL1 with its own secure console.
- Cache maintenance + barrier code for real (non-TCG-forgiving) MMU
  semantics.
- `arch/aarch64/machine.rs`: the `virt`/`sbsa-ref` board abstraction.

## Phases 12–14 — Hypervisor-assist hardening; doorbell-wakeup queues

`4625c58`, `12eef01`, `328c3b2`

- Message-queue ping demo driven entirely through the `Hypervisor`
  trait boundary.
- Doorbell-wakeup message queues: sender and receiver decouple (no
  polling) via SGI-driven wakeup.

## Phase 11 — SMP; TCP/UDP socket layer

`903ce3e`, `f22e165`, `9300be3`, `8395d39`, `1fdbb49`, `db104c0`

- `smp.rs`: PSCI `CPU_ON` secondary bring-up, per-CPU state, and a
  shared runqueue — culminating in real tasks being preempted onto
  idle CPUs 1–3, i.e. genuine SMP runqueue competition rather than a
  single core doing all the work.
- `libtanix-net` + `servers/net`: a full TCP/UDP socket layer.

## Phase 10 — VirtIO 1.0/PCI, virtio-net; IRQ-driven I/O

`7eacde5`

- Moved from legacy virtio-mmio to modern VirtIO 1.0 over PCI for the
  network path; `libtanix-drv`'s PCI/virtio-pci/vring plumbing.
- `SYS_MAP_DEVICE`, `SYS_IRQ_PENDING`: capability-style device-MMIO
  mapping and interrupt polling for userspace drivers.

## Phase 9 — RAMFS, shell, keyboard

`3bc9621`

- `servers/ramfs` + `servers/shell`: a ramdisk-backed filesystem and an
  interactive shell driven by injected virtio-keyboard events.
- `SYS_EXEC`: apps `exec`'d on demand from embedded images.

## Phase 8 — Window/compositor service

`d262c33`

- `servers/wm`: window manager / compositor managing z-order and damage
  regions across multiple concurrent apps (`counter`, `clock`,
  `ui-demo`).
- `SYS_SHARE_FRAMES` / `SYS_UNSHARE_FRAMES` / `SYS_SLEEP`.

## Phase 7 — Preemptive priority scheduler + device IRQs

`16f3f57`

- Replaced cooperative scheduling with a preemptive, priority-based
  scheduler; the 1 kHz EL1 tick now preempts EL0 tasks.
- `SYS_WAIT_IRQ` / `SYS_YIELD`; the virtio-gpu path becomes
  interrupt-driven instead of polled.

## Phase 6 — EL0 servers, SVC syscalls, per-task address spaces

`36a1484`

- Servers move to real EL0 with their own address spaces instead of
  running inside the kernel's; the syscall table takes its current
  SVC-based shape.

## Phase 5 — Display/UI stack

`b7cc15c`

- `servers/display`: virtio-gpu framebuffer + virtio-tablet pointer
  driver over legacy virtio-mmio.
- `servers/libtanix-ui` + `servers/ui-demo`: a pointer-reactive demo app
  (button + paint canvas) driven by real QMP-injected mouse input.

## Phase 4 — Minix-style servers

`409149a`

- `init`/`pm`/`mem`/`dev`/`worker`: the first Minix-style server set
  over synchronous kernel IPC (`SYS_SEND`/`SYS_RECEIVE`), plus a fix to
  the VM yield/resume boot-argument handoff.

## Phase 3 — VirtIO transport kernel ↔ Zephyr-stub

`2307c7f`, `2f06f77`

- `virtio/channel.rs` + `virtio/transport.rs`: a shared-memory virtqueue
  transport and a Print/Echo message protocol between the kernel and
  `servers/zephyr-stub`.

## Phases 1–2 — Bootstrap; hypervisor backend abstraction

`d7841f9`

- Initial `#![no_std]` boot on QEMU `virt`: EL2→EL1 drop, UART,
  exception vectors, GICv3, timer, MMU, frame allocator.
- `hypervisor/backend.rs`: the `Hypervisor` trait, modelled on Gunyah's
  API shape, with a `BareMetalBackend` cooperative-vCPU implementation.
