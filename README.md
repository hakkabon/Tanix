# Tanix

A Rust microkernel for AArch64 — a research PoC inspired by the Minix
philosophy (a tiny kernel, most OS services in unprivileged processes) with
an eye on automotive/embedded hypervisor use cases (Qualcomm Gunyah, Zephyr
guest RTOS, display-oriented UI).

Current state: **Phase 3 complete** — the kernel boots on QEMU's `virt`
machine, drops from EL2 to EL1, turns on the MMU, and runs a full VirtIO
message ping-pong with a guest "Zephyr-stub" VM embedded in the kernel image.

---

## Phase roadmap

| Phase | Goal | Status |
|-------|------|--------|
| 1 | AArch64 bare-metal bootstrap: UART, exception vectors, GICv3, timer | ✅ done |
| 2 | Memory (frame allocator, MMU) + hypervisor backend abstraction | ✅ done |
| 3 | VirtIO transport between kernel and guest VM (shmem + virtqueue) | ✅ done |
| 4 | Minix-style server processes (init, memory, device) | ⏳ next |
| 5 | Hypervisor assist (Gunyah-style) and display/UI | 🔭 planned |

---

## Repository layout

```
kernel/                  the microkernel itself (no_std, freestanding binary)
  src/main.rs            boot entry (_start asm stub), kmain, Phase-3 demo loop
  src/arch/aarch64/      CPU-level code
    uart.rs              PL011 driver + log backend
    exception.rs         VBAR_EL1 install, irq/sync dispatchers (vectors.s)
    vectors.s            exception vector table (16 slots, 128 B each)
    gic.rs               GICv3 distributor + redistributor init
    timer.rs             EL1 physical timer
    mmu.rs               TCR/MAIR configuration (MMU itself enabled later)
    boot.rs              CurrentEL / MPIDR helpers
  src/mem/
    frame.rs             bitmap frame allocator over the 256 MiB DDR
    page_table.rs        4-level page tables; pre-map + MMU enable
  src/hypervisor/
    mod.rs               backend selection (BareMetal vs Gunyah)
    backend.rs           Hypervisor trait (vm_create/start/resume, shmem, doorbell)
    doorbell.rs          SGI doorbell register/dispatch
  src/virtio/            shared-memory transport
    channel.rs           Print/Echo message format (opcodes, framing)
    transport.rs         kernel-side virtqueue driver (send/poll)
  src/vm/
    mod.rs               VmRuntime: cooperative vCPU pair (kernel ↔ guest)
    loader.rs            ELF64 + raw-binary loader
    shmem.rs             shared-memory allocator
  src/sched/task.rs      register-frame context switch (switch.s asm)
  link.ld                kernel image layout (load at 0x4008_0000)
  build.rs               linker args; embeds the stub binary path
servers/zephyr-stub/     the guest: a tiny Zephyr-like RTOS PoC
  src/main.rs            cooperative device loop, VirtIO device side
servers/init/            Phase-4 placeholder
scripts/qemu.sh          QEMU runner (debug/release, embed or fallback)
scripts/gdb.sh           QEMU + GDB server attach
justfile                 build/run/lint recipes
```

---

## Boot sequence

1. QEMU starts the `-kernel` image at **EL2** (`virt` machine with
   `virtualization=on` and no EL3 firmware). The kernel, however, is written
   for EL1 — so `_start` is a tiny assembly stub (`main.rs`, `global_asm!`)
   that:
   - reads `CurrentEL`;
   - from EL3: configures `SCR_EL3` (RW/HCE/NS) and erets to EL2;
   - from EL2: configures `HCR_EL2.RW` (AArch64 at EL1), zeroes `SCTLR_EL2`,
     and erets to EL1h (`SPSR_EL2` = 0x3C5);
   - sets SP to `__stack_top` **before any stack use** — the compiler
     prologue of the Rust entry would otherwise store through SP=0 and abort
     on the very first instruction;
   - branches to `kmain_entry` (zeroes BSS, calls `kmain`).
2. `kmain` runs: UART + log → exception vectors → TCR/MAIR → GICv3 →
   timer → frame allocator → **MMU enable** → hypervisor backend →
   Phase-3 VirtIO demo.

## Memory map (QEMU virt)

| Region | Address | Notes |
|--------|---------|-------|
| DDR | `0x4000_0000 .. 0x5000_0000` | 256 MiB; kernel loaded at `0x4008_0000` |
| Stack | just below `0x4008_0000` | 64 KiB window below the kernel image |
| GICv3 distributor | `0x0800_0000` | |
| GICv3 redistributors | `0x080A_0000` | per-CPU frames |
| PL011 UART | `0x0900_0000` | |

When the MMU is enabled (`mem::page_table::enable`), the **entire DDR** plus
the GIC and UART windows are pre-mapped as 2 MiB block descriptors *before*
the MMU bit is set. Every frame later handed out by the allocator (shared
memory, guest RAM, page tables) is therefore already mapped — this avoids
the classic "who maps the page-table pages?" chicken-and-egg problem.

---

## Hypervisor backend

`kernel/src/hypervisor/` defines a `Hypervisor` trait with `vm_create`,
`vm_start`, `vm_resume`, `mem_share`-style shmem allocation, and doorbell
primitives — a 1:1 sketch of the Qualcomm Gunyah API.

- `BareMetalBackend` (default): a cooperative "hypervisor" for the PoC —
  guest and kernel are a single vCPU pair that hand control to each other
  via register-frame context switches. No hardware virtualisation is needed.
- `GunhyBackend`: probed via an SMCCC-style HVC at boot, **gated behind the
  `gunyah` cargo feature** — on bare metal an HVC at EL1 is UNDEFINED and
  would crash the boot, so it must only be enabled when actually running
  under Gunyah.

## VM manager: cooperative vCPU pair

`kernel/src/vm/mod.rs` runs the guest without `eret`/HVC:

- `create_vm` allocates guest RAM (1 MiB), zeroes it, loads the stub image,
  and builds a `VmRuntime` holding two register contexts: the kernel's and
  the guest's.
- `start_vm` fills the guest context (`entry`, `sp = ram_top`) with boot
  arguments in registers, then context-switches into the guest:
  - `x4` — shared-memory (VirtQueue) physical base
  - `x5` — the kernel's `vm_yield_entry` function address
  - `x6` — the guest-context pointer
- The guest processes virtqueue entries and hands control back with
  `br x5` (`vm_yield_entry`), which saves the guest context and restores the
  kernel context. `resume_vm` switches back into the guest context.

Because the two contexts live on the same CPU, the exchange is fully
synchronous and race-free: the guest cannot run while the kernel prepares
the next message, and vice versa.

---

## VirtIO transport (Phase 3)

Shared-memory layout (4 pages = 16 KiB, allocated by `vm::shmem`):

| Offset | Size | Content |
|--------|------|---------|
| `0x000` | 48 B | `VirtqueueConfig` header (queue size, ring/buffer phys addrs, magic) |
| `0x040` | 256 B | descriptor table (16 × 16 B) |
| `0x140` | 32 B | avail ring (16 × 2 B) |
| `0x1000` | 256 B | used ring (16 × 16 B) |
| `0x1100` | 4 KiB | data buffers (16 × 256 B) |

Protocol (`virtio/channel.rs`): the kernel is the *driver* side, the guest
stub the *device* side.

- **Print** (opcode `0x01`): kernel → guest, text payload in a data buffer.
- **Echo** (opcode `0x02`): guest → kernel, reports the printed byte count.

Demo flow in `kmain` (3 rounds): `send_print` (posts to avail ring,
"doorbell" SGI for the future Gunyah path) → `resume_vm` (guest processes
the entry, prints, writes an Echo into the used ring, yields) →
`poll_replies` (kernel reads the used ring). Every round logs
`phase 3: round N — Echo received`.

The guest (`servers/zephyr-stub`) is a no_std freestanding binary built to
the same `aarch64-unknown-none` target and **embedded into the kernel image**
at compile time via `include_bytes!` (path emitted by `kernel/build.rs`).
Without the `embed-zephyr-stub` feature, a tiny fallback stub is used that
boots, yields once, and never answers — so the demo completes with warnings
instead of hanging.

---

## Build & run

Prerequisites: Rust `nightly-2026-07-01` (pinned by `rust-toolchain.toml`),
the `aarch64-unknown-none` target, QEMU (`brew install qemu` /
`apt install qemu-system-arm`), and optionally `just`.

```sh
# Phase 3 VirtIO demo (real stub embedded)
just qemu-phase3

# or manually:
cargo build --package tanix-zephyr-stub --target aarch64-unknown-none
cargo build --package tanix-kernel --target aarch64-unknown-none \
    --features embed-zephyr-stub
./scripts/qemu.sh
```

Important QEMU flags (already in `scripts/qemu.sh` / `gdb.sh`):

- `virtualization=on` — boots the kernel at EL2; the kernel drops itself
  to EL1 at entry (and this is where the Gunyah HVC path will live later).
- `gic-version=3` — the kernel's GIC driver is GICv3 (distributor +
  redistributor at `0x080A_0000`); without this the machine defaults to
  GICv2 and the redistributor access aborts.

Recipes: `just kernel` (fallback stub), `just kernel-embed` (real stub),
`just qemu-release`, `just debug` (GDB), `just lint-all` (clippy with
`-D warnings`).

---

## Known limitations

- Single CPU, cooperative scheduling only; no SMP, no interrupts for the
  guest (the SGI doorbell is registered but the demo communicates via the
  yield pair).
- Page tables pre-map the whole DDR RWX; permissions are tightened in
  Phase 4.
- The EL2 stage is a one-way drop for now; no hypervisor services exist yet
  (`hvc` handling at EL1 is only exercised under the `gunyah` feature).

## License

MIT.
