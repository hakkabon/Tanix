# Tanix

A Rust microkernel for AArch64 — a research PoC inspired by the Minix
philosophy (a tiny kernel, most OS services in unprivileged processes) with
an eye on automotive/embedded hypervisor use cases (Qualcomm Gunyah, Zephyr
guest RTOS, display-oriented UI).

Current state: **Phase 5 complete** — the kernel boots on QEMU's `virt`
machine, drops from EL2 to EL1, turns on the MMU, runs a full VirtIO
message ping-pong with a guest "Zephyr-stub" VM, boots a set of
Minix-style server processes (init, pm, mem, dev, worker) that talk to
each other over synchronous kernel IPC, and drives a display stack
end-to-end: a `display` server drives virtio-gpu (framebuffer) and
virtio-tablet (pointer) over the QEMU virtio-mmio transport, and a
`ui-demo` app draws a button + paint-canvas UI that reacts to real mouse
input injected through QEMU's QMP monitor.

---

## Phase roadmap

| Phase | Goal | Status |
|-------|------|--------|
| 1 | AArch64 bare-metal bootstrap: UART, exception vectors, GICv3, timer | ✅ done |
| 2 | Memory (frame allocator, MMU) + hypervisor backend abstraction | ✅ done |
| 3 | VirtIO transport between kernel and guest VM (shmem + virtqueue) | ✅ done |
| 4 | Minix-style server processes (init, pm, mem, dev, worker) | ✅ done |
| 5 | Display/UI stack: virtio-gpu framebuffer + virtio-tablet pointer, driven by a Minix-style display server | ✅ done |
| 6 | Hypervisor assist (Gunyah-style) | 🔭 planned |
| 7 | Preemptive priority scheduler + device IRQs (timer tick, GIC PPIs/SPIs, `SYS_WAIT_IRQ` for virtio devices) | ✅ done |

---

## Repository layout

```
kernel/                  the microkernel itself (no_std, freestanding binary)
  src/main.rs            boot entry (_start asm stub), kmain, demo loops
  src/arch/aarch64/      CPU-level code
    uart.rs              PL011 driver + log backend
    exception.rs         VBAR_EL1 install, irq/sync dispatchers (vectors.s)
    vectors.s            exception vector table (16 slots, 128 B each)
    gic.rs               GICv3 distributor + redistributor init
    timer.rs             EL1 physical timer
    mmu.rs               TCR/MAIR configuration (MMU itself enabled later)
    boot.rs              CurrentEL / MPIDR helpers
  src/mem/
    frame.rs             bitmap frame allocator over the 256 MiB DDR (+ reserved zones)
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
  src/ipc/               synchronous send/receive channels + syscall table
  src/sched/             cooperative scheduler; register-frame context switch
    task.rs              Task / Context (switch.s asm), spawn_server
  src/server.rs          server registry: embed + spawn by name, region reservation
  link.ld                kernel image layout (load at 0x4008_0000)
  build.rs               linker args; emits embedded-binary paths
servers/zephyr-stub/     the Phase-3 guest: a tiny Zephyr-like RTOS PoC
  src/main.rs            cooperative device loop, VirtIO device side
servers/libtanix-sys/    shared no_std crate: syscall table, Message/ABI (BootInfo)
servers/init/            root server: spawns pm/mem/dev, drives the demo
servers/pm/              process manager (spawn/exec via syscall)
servers/mem/             memory service (grant/query)
servers/dev/             device service (text I/O)
servers/worker/          worker binary, exec'd by pm at runtime
servers/display/         Phase-5 display server: virtio-mmio transport (virtio.rs),
                         virtio-gpu driver (gpu.rs), virtio-tablet driver (input.rs)
servers/libtanix-ui/     shared UI helpers for the demo apps
servers/ui-demo/         Phase-5 demo app: button + paint canvas, pointer-reactive
servers/link.ld          server link script (fixed LINK_BASE layout)
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
   Phase-3 VirtIO demo → Phase-4 server demo → Phase-5 display stack.

## Memory map (QEMU virt)

| Region | Address | Notes |
|--------|---------|-------|
| DDR | `0x4000_0000 .. 0x5000_0000` | 256 MiB; kernel loaded at `0x4008_0000` |
| Stack | just below `0x4008_0000` | 64 KiB window below the kernel image |
| Server regions | `0x4070_0000 .. 0x4080_0000` | 512 KiB reserved for embedded servers |
| Framebuffer | `0x407e_2000` | virtio-gpu scanout surface (1280×800×4) |
| GICv3 distributor | `0x0800_0000` | |
| GICv3 redistributors | `0x080A_0000` | per-CPU frames |
| PL011 UART | `0x0900_0000` | |
| virtio-mmio | `0x0A00_0000 .. 0x0C00_0000` | 32 slots × 0x200; gpu at slot 31, tablet at slot 30 |

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

The boot arguments are caller-saved registers, so the kernel re-establishes
them on *every* entry — first launch and each resume — because the kernel's
own execution between a yield and its resume clobbers them. The switch
itself (`enter_guest`) is a single fused `asm!` block: it loads x4/x5/x6,
prepares x0/x1, and `bl`s `context_switch`, so no compiler-generated
instruction can sit between the register loads and the switch.

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
resets SP to the top of the guest RAM and loops `br x5` forever, yielding
control back to the kernel on every resume — the demo then completes with
warnings instead of hanging.

---

## Minix-style servers (Phase 4)

The kernel boots a set of independent server binaries — `init`, `pm`
(process manager), `mem` (memory service), `dev` (device service), and
`worker` — as cooperative scheduler tasks, each in its own private 128 KiB
memory region with its own stack.

- Servers are separate no_std crates under `servers/`, **embedded into the
  kernel image** at compile time (`embed-servers` feature, paths emitted by
  each crate's `build.rs`). They are plain statically-linked executables
  linked at **fixed** physical bases (`SERVER_BASES` in `kernel/src/server.rs`,
  clear of the kernel image and Phase-3 guest RAM).
- At spawn, the kernel zeroes the region, loads the ELF, creates the task,
  and hands it a `BootInfo` block (syscall table + own task id) preloaded
  into its callee-saved `x19`.
- Servers never link against the kernel: they communicate only through the
  syscall table and through synchronous `send`/`receive` IPC
  (`kernel/src/ipc/`, rendezvous-style, `MSG_MAX_BYTES = 64`).

Demo flow in `kmain`: spawn `init` → enter the scheduler → `init` spawns
`pm`/`mem`/`dev` and exercises each service over IPC (dev prints, mem grants
memory, pm execs the `worker` binary), then the demo completes when every
server has blocked or finished.

---

## Display stack (Phase 5)

The display stack is another pair of embedded servers, spawned after the
Phase-4 set:

```
ui-demo ── M_DISPLAY_FILL_RECT/FLUSH/TICK ──▶ display server ──▶ virtio-gpu
                                                 │               (framebuffer)
                                                 └──▶ virtio-tablet
                                                      (pointer events)
```

### virtio-mmio transport

`servers/display/src/virtio.rs` is a small virtio **legacy** transport
driver for the QEMU virtio-mmio device type: it probes the 32 slots at
`0x0A00_0000` (`REG_MAGIC`/`REG_DEVICE_ID`), resets, negotiates features,
configures a queue, writes `DRIVER_OK`, and then moves buffers through the
descriptor/avail/used rings — including a `submit` path that chains a
read-only descriptor sequence and a write-only response, and a
`drain_used` path for event buffers. In the current QEMU layout the
virtio-gpu sits at slot 31 (`0x0A00_3E00`, device id 16) and the
virtio-tablet at slot 30 (`0x0A00_3C00`, device id 18).

### virtio-gpu driver (`gpu.rs`)

- Probes and initialises the control queue, then runs
  `VIRTIO_GPU_CMD_GET_DISPLAY_INFO` / `RESOURCE_CREATE_2D` /
  `RESOURCE_ATTACH_BACKING` to obtain a 1280×800 BGRA framebuffer.
- QEMU 11 keeps resource images in host memory: the guest must send
  `VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D` (0x105) before every
  `VIRTIO_GPU_CMD_RESOURCE_FLUSH` (0x104), or the screen stays black —
  `flush()` does transfer-then-flush.
- The framebuffer pages are carved out of the server region
  (`0x407e_2000`, see above); the frame allocator reserves all server
  regions so the GPU's attached backing pages can never collide with a
  live server image.

### virtio-tablet driver (`input.rs`)

- Device id 18, one event queue (index 0); the device only starts
  reporting after `DRIVER_OK`. `open()` fills the ring with 64 independent
  writable 32-byte event buffers (`add_empty_buffers`).
- Events are 8-byte LE `{u16 type; u16 code; u32 value}` records, batched
  and terminated with `EV_SYN`/`SYN_REPORT`: `EV_ABS ABS_X/ABS_Y` carry the
  pointer (0..0x7FFF, mapped to pixels by the display server) and
  `EV_KEY BTN_TOUCH`/`BTN_LEFT` the button state.
- `poll()` drains the used ring (up to 8 records per tick), updates the
  `Pointer {x, y, buttons}`, and re-arms the consumed buffers.

### M_DISPLAY protocol (`servers/libtanix-sys/src/abi.rs`)

The display server serves one request per `receive` wake-up (the
scheduler is fully cooperative; the timer tick only disarms the timer
event, so the service loop is receive-driven):

| mtype | direction | payload |
|-------|-----------|---------|
| `M_DISPLAY_GET_MODE` | app → display | — |
| `M_DISPLAY_MODE_REPLY` | display → app | `data[0,1]` = width, height |
| `M_DISPLAY_FILL_RECT` | app → display | `data[0..3]` = x, y, w, h; `data[4..6]` = r, g, b |
| `M_DISPLAY_FLUSH` | app → display | transfer + flush the framebuffer |
| `M_DISPLAY_TICK` | app → display | poll tablet; reply = pointer |
| `M_DISPLAY_TICK_REPLY` | display → app | `data[0,1,2]` = px, py, buttons |

`ui-demo` is a pointer-reactive app: a top-right button toggles the
background colour, pressing or dragging elsewhere paints amber dots, and a
white ring cursor follows the tablet. It redraws only when the pointer
moved or the button state changed, and its TICK request/response cadence
doubles as the input polling loop.

---

## Preemptive scheduler + device IRQs (Phase 7)

Phase 7 replaces the cooperative scheduler with a preemptive,
priority-based one, and turns the virtio-gpu path interrupt-driven:

- **Priorities** — each server gets a fixed priority (lower number runs
  first): display `32`, ui-demo `96`, the Phase-4 servers `128`, the `hog`
  demo `192`, idle `255`.  Equal priorities rotate round-robin on ticks.
- **Timer tick** — `timer.rs` arms the EL1 physical timer (GIC **PPI 26**
  = CNTPNSIRQ on QEMU `virt`; the old PPI 30 was the EL2-only CNTHP and
  never fired) for a periodic 1 ms tick.  `CNTKCTL_EL1 = 0b111` grants EL1
  access to the CNTP_EL0 registers.
- **Preemption** — every tick that lands on an EL0 task runs the scheduler
  (`tick_preempt`); ticks inside the kernel (e.g. the `SYS_WAIT_IRQ` wait
  loop) only count.  `IRQ_ENTRY` in `vectors.s` passes `from_el0` to
  `irq_handler`, and a preempted task resumes through its saved IRQ frame
  (the kernel stack the frame lives on is the task's own).
- **Syscall-tail reschedule** — every syscall return re-evaluates the run
  queue; a strictly higher-priority task woken by the syscall runs
  immediately (equal-priority rotation stays on the tick, which avoids
  ping-ponging between suspended syscall tails).
- **`SYS_WAIT_IRQ`** — a server blocks until its device interrupt arrives.
  The kernel enables the IRQ in the GIC lazily (Group 1NS — the kernel
  runs non-secure EL1, so Group 0 interrupts would never be delivered),
  records the delivery in a pending bitmap, and the waiter sleeps in `wfi`
  with IRQs unmasked.  Level-triggered virtio-mmio lines can't lose
  completions: an IRQ that beats the wait leaves the line high.
- **virtio-gpu interrupt-driven** — `Device::irq()` computes the SPI
  (`48 + slot`), and `submit()` blocks on `SYS_WAIT_IRQ` then deasserts
  the line via INT_STATUS/INT_ACK before draining the used ring.
- **`hog`** — a lowest-priority CPU-bound server (spin + periodic log +
  `yield_cpu`).  Under the old cooperative scheduler a spinning task would
  starve the system forever; now the tick keeps preempting and the log
  shows `irq: tick #N` + `sched: preempt` lines while hog spins.

```sh
# Phase 7 demo: preemptive scheduler + IRQ-driven GPU (recommended)
just qemu-phase7 -qmp unix:/tmp/qmp.sock,server=on,wait=off
```

---

## Build & run

Prerequisites: Rust `nightly-2026-07-01` (pinned by `rust-toolchain.toml`),
the `aarch64-unknown-none` target, QEMU (`brew install qemu` /
`apt install qemu-system-arm`), and optionally `just`.

```sh
# Phase 7 demo: preemptive scheduler + IRQ-driven GPU (recommended) — pass
# the QMP socket to inject mouse input and take screenshots
just qemu-phase7 -qmp unix:/tmp/qmp.sock,server=on,wait=off

# Phase 5 demo: display stack + UI (recommended) — pass the QMP socket to
# inject mouse input and take screenshots (see "Driving the UI" below)
just qemu-phase5 -qmp unix:/tmp/qmp.sock,server=on,wait=off

# Phase 4 demo: VirtIO ping-pong + Minix-style server set
just qemu-phase4

# Phase 3 VirtIO demo only (real stub embedded)
just qemu-phase3

# Fallback demo (no embedded guest/servers — builds and boots anywhere)
just qemu

# or manually:
cargo build --package tanix-zephyr-stub --target aarch64-unknown-none
cargo build --package tanix-libsys --package tanix-libtanix-ui \
    --package tanix-init --package tanix-pm --package tanix-mem \
    --package tanix-dev --package tanix-worker --package tanix-display \
    --package tanix-ui-demo --package tanix-hog --target aarch64-unknown-none
cargo build --package tanix-kernel --target aarch64-unknown-none \
    --features embed-zephyr-stub,embed-servers
./scripts/qemu.sh -device virtio-gpu-device -device virtio-tablet-device
```

Important QEMU flags (already in `scripts/qemu.sh` / `gdb.sh`):

- `virtualization=on` — boots the kernel at EL2; the kernel drops itself
  to EL1 at entry (and this is where the Gunyah HVC path will live later).
- `gic-version=3` — the kernel's GIC driver is GICv3 (distributor +
  redistributor at `0x080A_0000`); without this the machine defaults to
  GICv2 and the redistributor access aborts.
- `-device virtio-gpu-device -device virtio-tablet-device` — the Phase-5
  display devices, required by the display server (`just qemu-phase5`).
- `-qmp unix:...` — QMP monitor for screenshots and input injection (the
  display server runs with no input until you inject events this way).

### Driving the UI from QMP

The guest is `-nographic`, so mouse input is injected over QMP (any QMP
client works, e.g. `socat - UNIX-CONNECT:/tmp/qmp.sock`). Tablet abs
values are 0..0x7FFF; convert pixels with `x × 32767 / 1280` and
`y × 32767 / 800` (e.g. screen centre (640, 400) → (16384, 16384)):

```json
{"execute":"qmp_capabilities"}
{"execute":"input-send-event","arguments":{"events":[
  {"type":"abs","data":{"axis":"x","value":16384}},
  {"type":"abs","data":{"axis":"y","value":16384}}]}}
{"execute":"input-send-event","arguments":{"events":[
  {"type":"btn","data":{"button":"left","down":true}}]}}
{"execute":"input-send-event","arguments":{"events":[
  {"type":"btn","data":{"button":"left","down":false}}]}}
{"execute":"screendump","arguments":{"filename":"/tmp/shot.ppm"}}
```

Notes: explicit `{"type":"sync"}` events are rejected by QEMU 11 —
synchronisation is automatic; and screendump writes PPM despite any `.png`
extension.

Recipes: `just kernel` (fallback stub), `just kernel-embed` (real stub),
`just kernel-phase4` (servers), `just kernel-phase5` (servers + display
stack), `just qemu-release`, `just debug` (GDB), `just lint-all` (clippy
with `-D warnings`).

---

## Known limitations

- Single CPU, cooperative scheduling only; no SMP, no interrupts for the
  guest (the SGI doorbell is registered but the demo communicates via the
  yield pair). The display service loop is receive-driven: there is no
  device interrupt for the tablet, so the pointer is sampled on the app's
  TICK cadence.
- Page tables pre-map the whole DDR RWX; permissions are not yet tightened
  per server region (server RAM zones are reserved from the frame
  allocator, but not yet marked read-only to other tasks).
- The EL2 stage is a one-way drop for now; no hypervisor services exist yet
  (`hvc` handling at EL1 is only exercised under the `gunyah` feature).
- virtio is used in legacy (pre-1.0) mode, single queue per device;
  multi-queue and modern (PCI) transports are not implemented.

## License

MIT.
