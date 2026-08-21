# Tanix

A Rust microkernel for AArch64 — a research PoC inspired by the Minix
philosophy (a tiny trusted core, most OS services in unprivileged
processes) with an eye on automotive/embedded hypervisor use cases
(Gunyah-style VM management, a Zephyr-like RTOS co-tenant, TrustZone
secure services, and a display/UI stack).

**Current state: Phase 21 complete** (~23.6K lines of Rust across the
kernel and 25 workspace crates). The kernel boots on two QEMU machines —
`virt` (EL2→EL1 drop, PSCI from QEMU's fake EL3 firmware) and `sbsa-ref`
(true EL3 reset, with Tanix's own EL3 monitor supplying PSCI and
TrustZone secure services) — brings up SMP, a preemptive priority
scheduler, demand paging/COW, a capability-gated MMIO path for
userspace drivers, a socket layer (TCP/UDP over virtio-net), a FAT16
filesystem server over virtio-blk, a composited display/window-manager
stack, a shell over a ramdisk, and a co-tenant VM scheduler running a
Zephyr-style RTOS guest alongside the native servers. Phase 22 (CI
harness, docs, syscall-coverage pass) is the next open item — see
[Known limitations](#known-limitations).

---

## Roadmap

The original 5-phase sketch (bootstrap → Gunyah port → VirtIO → Minix
servers → display) was superseded early on by a longer, more granular
plan once the project outgrew the PoC stage. Phases actually implemented
in this repository:

| Phase | Goal | Status |
|-------|------|--------|
| 1 | AArch64 bare-metal bootstrap: UART, exception vectors, GICv3, timer | ✅ |
| 2 | Memory (frame allocator, MMU) + hypervisor backend abstraction | ✅ |
| 3 | VirtIO transport between kernel and a guest VM (shmem + virtqueue) | ✅ |
| 4 | Minix-style server processes (init, pm, mem, dev, worker) over kernel IPC | ✅ |
| 5 | Display/UI stack: virtio-gpu framebuffer + virtio-tablet pointer | ✅ |
| 6 | Hypervisor-assist primitives (Gunyah-style message queues, doorbells) | ✅ |
| 7 | Preemptive priority scheduler + device IRQs (`SYS_WAIT_IRQ`, GIC PPIs/SPIs) | ✅ |
| 8 | EL0 servers with per-task address spaces; window/compositor service | ✅ |
| 9 | RAMFS + shell + keyboard input; apps `exec`'d from embedded images | ✅ |
| 10 | VirtIO 1.0 / PCI transport + virtio-net; IRQ-driven I/O stack | ✅ |
| 11 | SMP (PSCI `CPU_ON`, per-CPU state, shared runqueue); TCP/UDP socket layer | ✅ |
| 12–13 | Hypervisor-assist hardening; message-queue ping demo over the `Hypervisor` trait | ✅ |
| 14 | Doorbell-wakeup message queues (decoupled send/resume, no polling) | ✅ |
| 15 | Real hardware target groundwork: cache maintenance, barriers, EL3 monitor for TrustZone | ✅ |
| 16 | `sbsa-ref` boot (EL3 reset → monitor → EL2 → EL1); TrustZone secure payload | ✅ |
| 17 | Secure services over SMC (secure storage, keybox, attestation); server attestation | ✅ |
| 18 | UEFI/ACPI boot path (RSDP → MADT/SPCR/MCFG); EFI handoff | ✅ |
| 19 | Demand paging, copy-on-write, lazy stack growth; capability-gated MMIO for drivers | ✅ |
| 20 | FAT16 filesystem server over virtio-blk; full TCP; multi-threading groundwork | ✅ |
| 21 | Co-tenant vCPU scheduler running a Zephyr-style RTOS guest alongside native servers | ✅ (fresh — see below) |
| 22 | QEMU CI harness (`virt` + `sbsa-ref`, SMP + net regression boots), docs pass, syscall coverage | ⏳ open |

Phase 21 is functionally complete but young: the most recent commit
(`a794802`) fixes a guest-panic-inside-panic-handler crash in
`rtos-guest`, so treat the co-tenant scheduler as freshly stabilized
rather than long-soaked. MSI-X/ITS and NVMe (mentioned in early phase-18
notes) were not pursued — ACPI/UEFI landed via the MADT/SPCR/MCFG path
instead, and storage stayed on virtio-blk.

Two originally-separate tracks converged during the project: the
Phase-2/3 idea of "port to Gunyah" became, in practice, a from-scratch
`Hypervisor` trait (`kernel/src/hypervisor/`) modelled closely on
Gunyah's API shape (VM create/start/resume, shared-memory registration,
doorbells, message queues) but implemented as a cooperative/preemptive
in-kernel scheduler rather than a second exception level — see
[Hypervisor backend](#hypervisor-backend--co-tenant-scheduler) below.

---

## Repository layout

```
kernel/                    the microkernel itself (no_std, freestanding binary)
  src/main.rs               boot entry (_start asm stub), kmain, per-phase demo wiring
  src/arch/aarch64/
    uart.rs                 PL011 driver + log backend
    exception.rs             VBAR_EL1 install, sync/irq dispatch, user-fault triage
    vectors.s                exception vector table
    gic.rs                   GICv3 distributor + redistributor
    timer.rs                 EL1 physical timer (1 kHz preemption tick)
    mmu.rs                    TCR/MAIR configuration, page-table enable
    boot.rs                   CurrentEL / MPIDR helpers
    cache.rs                  cache maintenance + barriers (DMA ownership transfer)
    psci.rs                   PSCI over SMC (CPU_ON secondary bring-up)
    monitor.rs                Phase 16: EL3 monitor — TrustZone, secure services, PSCI
    fdt.rs / acpi.rs / efi.rs Phase 18: device-tree and ACPI/UEFI machine discovery
    machine.rs                Phase 16: `virt` vs `sbsa-ref` board abstraction
  src/mem/
    frame.rs                  bitmap frame allocator
    page_table.rs             4-level page tables; identity map + MMU enable
    vm_fault.rs                Phase 19: demand paging / COW / stack-growth fault resolver
  src/hypervisor/
    mod.rs / backend.rs       `Hypervisor` trait: vm_create/start/resume, shmem, doorbell
    doorbell.rs                SGI doorbell register/dispatch
    message_queue.rs / msgq_abi.rs   Gunyah-style message-queue object + shared layout
  src/virtio/                shared-memory VirtIO transport (kernel side)
  src/vm/
    mod.rs                     VmRuntime: vCPU pair (kernel ↔ guest) context switch
    sched.rs                   Phase 21: co-tenant vCPU scheduler (multi-VM round robin)
    loader.rs                  ELF64 + raw-binary loader
    shmem.rs                   shared-memory allocator
  src/ipc/                    synchronous send/receive channels + the syscall table (27 calls)
  src/sched/                  preemptive priority scheduler; register-frame context switch
  src/smp.rs                  Phase 11: per-CPU state, secondary bring-up
  src/irq.rs                  IRQ dispatch, pending bitmap, SYS_WAIT_IRQ wake path
  src/server.rs               server registry: embed + spawn by name, region reservation
  link.ld / link-sbsa.ld      kernel image layout for `virt` and `sbsa-ref`
  build.rs                    linker args; emits embedded-binary paths (TANIX_LINK_SHIFT aware)

servers/
  libtanix-sys/               shared no_std crate: syscall table, Message/ABI, BootInfo
  libtanix-drv/                shared userspace driver library: PCI, virtio-pci, vring, blk, net
  libtanix-net/                TCP/UDP protocol stack (Phase 11/20)
  libtanix-fs/                 FAT16 filesystem library (Phase 20)
  libtanix-ui/                 shared UI helpers for display apps

  init/ pm/ mem/ dev/ worker/  Phase 4 Minix-style core: process mgmt, memory grants, device I/O
  display/                     Phase 5: virtio-gpu + virtio-tablet driver server
  wm/                          Phase 8: window manager / compositor
  ui-demo/ counter/ clock/     demo GUI apps (button+canvas, counter, clock) over the wm
  hog/                         CPU-bound demo server exercising preemption
  ramfs/ shell/                Phase 9: ramdisk-backed fs + interactive shell
  net/                         Phase 10/11: virtio-net server, ARP/ICMP + TCP/UDP sockets
  fs/                          Phase 20: FAT16-over-virtio-blk file server
  sec/                         Phase 17: secure-storage/keybox/attestation demo (sbsa-ref only)
  ping/ pong/                  minimal IPC ping-pong demo servers
  zephyr-stub/                 Phase 3 guest: minimal VirtIO-speaking RTOS stub
  rtos-guest/                  Phase 21 guest: Zephyr-modelled RTOS (k_thread/k_sem/k_msgq/k_timer)

scripts/
  qemu.sh / qemu-sbsa.sh       QEMU runners for `virt` / `sbsa-ref`
  gdb.sh                       QEMU + GDB attach
  keyboard-demo.sh / mouse-demo.sh / net-test.sh   scripted QMP-driven demos + assertions
  mkfat16.py / elf2efi.py      FAT16 demo-volume builder / ELF→PE-COFF (EFI) converter

justfile                    build/run/lint recipes, one per phase milestone
```

---

## Boot sequence

Two independent boot paths share almost all of the kernel above the
entry stub:

**`virt`** (default): QEMU starts the `-kernel` image at EL2 (fake EL3
firmware supplies PSCI). `_start` reads `CurrentEL`, configures
`HCR_EL2.RW`, and erets to EL1h before any Rust code runs (SP must be
set before the compiler prologue touches the stack). `kmain` then brings
up UART → exception vectors → TCR/MAIR → GICv3 → timer → frame allocator
→ MMU enable → hypervisor backend → the embedded demo for whichever
`just qemu-phaseN` recipe built the image.

**`sbsa-ref`** (feature `sbsa-ref`, Phase 16+): CPUs reset at EL3
because `sbsa-ref` has no working PSCI of its own. Tanix's own
`monitor_el3_init` runs first: installs EL3 vectors, sets up the secure
payload in secure RAM, and only then drops the primary CPU to EL1
(secondaries park at EL3 until PSCI `CPU_ON` wakes them). From EL1
onward the boot converges with the `virt` path. This machine can also be
entered through UEFI (Phase 18): an EFI stub stashes the system-table
pointer, and `acpi.rs`/`fdt.rs` read the real hardware topology (GIC,
UART, PCIe ECAM) from firmware tables instead of compiled-in constants.

---

## Hypervisor backend & co-tenant scheduler

`kernel/src/hypervisor/` defines a `Hypervisor` trait shaped after
Gunyah's API (`vm_create`, `vm_start`, `vm_resume`, shared-memory
registration, doorbells, message queues). The shipped backend is a
**software vCPU scheduler**, not a second hardware exception level:
guest and kernel are register-frame contexts that hand control to each
other via context switches (`vm/mod.rs`), with no stage-2 translation or
EL2 guest isolation. This was a deliberate scope cut from the original
"port to real Gunyah" plan — the trait boundary is real, but a
hardware-backed implementation (stage-2 page tables, a genuine EL2
world) is future work.

Phase 21 extends the single kernel↔guest vCPU pair into an N-way
**co-tenant scheduler** (`vm/sched.rs`): the kernel round-robins over a
tenant table, and the EL1 tick preempts a tenant mid-guest (full frame
capture) exactly like it preempts a native task, resuming it later where
it stopped. The current guest is `rtos-guest`, a Zephyr-modelled RTOS
with `k_thread`/`k_sem`/`k_msgq`/`k_sleep`/`k_timer` primitives and its
own cooperative scheduler inside each time slice.

---

## Syscall surface

27 syscalls, added incrementally per phase (`kernel/src/ipc/syscall.rs`):
IPC (`SEND`/`RECEIVE`), process control (`SPAWN`/`EXEC`/`EXIT`/`WHO`),
memory (`ALLOC_FRAMES`/`FREE_FRAMES`/`SHARE_FRAMES`/`UNSHARE_FRAMES`,
Phase 19's `MAP_DEMAND`/`MAP_COW`/`MAP_CAP`), scheduling
(`YIELD`/`SLEEP`/`WAIT_IRQ`/`IRQ_PENDING`), device access
(`MAP_DEVICE`, `CACHE_SYNC`), and — `sbsa-ref` only — the Phase 17
secure-service quintet (`SEC_STORAGE_PUT/GET`, `KEYBOX_GEN/SEAL/UNSEAL`,
`ATTEST`), which round-trip through the EL3 monitor over SMC and return
`-1` on `virt` where no monitor exists.

---

## Machine targets

| | `virt` (default) | `sbsa-ref` (`--features sbsa-ref`) |
|---|---|---|
| Boot | `-kernel`, EL2 start, fake-firmware PSCI | EL3 reset, Tanix's own EL3 monitor + PSCI |
| RAM | 256 MiB @ `0x4000_0000` | window @ `0x100_0000_0000` (1 TiB) + 512 MiB secure-only @ `0x2000_0000` |
| GIC | GICv3 @ `0x0800_0000` | GICv3 @ `0x4006_0000` / `0x4008_0000` |
| Console | PL011 @ `0x0900_0000` | PL011 @ `0x6000_0000`, secure PL011 @ `0x6003_0000` |
| PCIe ECAM | `0x3F00_0000` | `0xF000_0000` |
| TrustZone | not modelled | EL3 monitor, secure payload, `sec` server demo |
| Firmware | none (or EFI, Phase 18) | EFI/ACPI (Phase 18) or DT |

The two targets build into **separate Cargo target directories**
(`target/` vs `target-sbsa/`, via `CARGO_TARGET_DIR` +
`TANIX_LINK_SHIFT`) so binaries linked at different base addresses never
collide.

---

## Build & run

Prerequisites: Rust `nightly-2026-07-01` (pinned by `rust-toolchain.toml`),
the `aarch64-unknown-none` target, QEMU (`brew install qemu` /
`apt install qemu-system-arm`), and `just`.

```sh
# Latest virt-machine milestone: SMP + full socket stack (Phase 11)
just qemu-phase11

# sbsa-ref: EL3 monitor/TrustZone + all machine-agnostic servers (Phase 16/17)
just qemu-sbsa

# sbsa-ref + Phase 21 co-tenant RTOS guest (the current head-of-line demo)
just qemu-sbsa-rtos

# Display/desktop milestones on virt
just qemu-phase5     # display server + ui-demo (virtio-gpu + virtio-tablet)
just qemu-phase7     # + preemptive scheduler, IRQ-driven GPU
just qemu-phase8     # + window manager, multiple composited apps
just qemu-phase9     # + ramfs + shell (virtio-keyboard)

# Scripted demos with QMP-injected input + assertions
just qemu-phase9-demo   # shell over injected keyboard events, screenshots
just qemu-phase8-mouse  # cursor-follows-injected-path screenshot
just net-test            # boots phase-11 kernel, drives UDP/TCP wire paths, asserts replies

# Fallback (no embedded guest/servers — builds and boots anywhere)
just qemu

# Quality
just lint-all   # clippy, whole workspace, warnings as errors
just check      # cargo check, kernel only
just debug      # QEMU + GDB attach
```

Each `just qemu-phaseN` recipe builds the exact server set that
milestone needs and embeds it in the kernel image (`embed-servers`
feature) — see `justfile` for the full recipe graph, since later
phases build on the server sets of earlier ones (e.g. `servers-phase11`
depends on `servers-phase10` depends on `servers-phase9`...).

### `sbsa-ref` notes

`qemu-sbsa` / `qemu-sbsa-rtos` boot through `scripts/qemu-sbsa.sh`,
which wires a second `-serial` chardev to `target-sbsa/sec.log` for the
secure-world console. Server binaries for this machine are built with
`TANIX_LINK_SHIFT=0xFFC2000000` into `target-sbsa/` so they land inside
the 1 TiB RAM window; `rtos-guest` is the one binary that does *not*
need the shift (it's loaded into kernel-allocated guest RAM at runtime,
like `zephyr-stub`).

---

## Known limitations

- **Phase 22 (CI, docs, syscall coverage) is open.** There is no
  `.github/workflows` CI harness yet, no `CHANGELOG.md`, and no
  automated regression suite beyond the scripted QMP demos
  (`net-test.sh`, `keyboard-demo.sh`, `mouse-demo.sh`) — these assert
  outcomes but aren't wired into any CI.
- **The hypervisor backend has no hardware isolation.** Tenants
  (`zephyr-stub`, `rtos-guest`) share the kernel's own EL1 address space
  and page tables; there is no EL2 stage-2 translation. A "real Gunyah"
  or "real EL2 world" story is future work, documented as a platform
  constraint in `vm/sched.rs`.
- **`rtos-guest` (Phase 21) is freshly stabilized.** The most recent
  commit fixed a guest panic-in-panic-handler crash; the co-tenant
  scheduler has not had extensive soak time.
- **Error handling is inconsistent in places.** ~70 `.unwrap()` and ~36
  `.expect()` calls remain across the kernel and servers, mostly in
  boot-time paths where a failure is arguably fatal anyway, but this
  hasn't been systematically audited.
- **Single-core is still the default demo path** for the display/UI
  milestones (phases 5, 7, 8, 9); SMP (Phase 11) is exercised on the
  socket-stack and later builds but the two aren't currently combined
  in one demo image.
- **VirtIO transports mix legacy and modern.** The Phase 3 kernel↔guest
  channel and early virtio-mmio devices (gpu/tablet/keyboard) are
  legacy (pre-1.0), single-queue; Phase 10+ (`net`, `fs`) moved to
  modern VirtIO 1.0 over PCI. Both styles coexist depending on which
  server you're looking at.
- **MSI-X/ITS and NVMe were dropped from scope.** The Phase 18 UEFI/ACPI
  work landed via MADT/SPCR/MCFG discovery; storage stayed on
  virtio-blk with a FAT16 server rather than moving to NVMe.

## License

MIT.