# Tanix kernel build recipes
# Usage: just <recipe>   (requires https://github.com/casey/just)

TARGET      := "aarch64-unknown-none"
KERNEL_PKG  := "tanix-kernel"
STUB_PKG    := "tanix-zephyr-stub"

# Default: build kernel (debug)
default: kernel

# ── Build ─────────────────────────────────────────────────────────────────────

# Build the Zephyr-stub guest binary (must be done before kernel-embed)
zephyr-stub:
    cargo build --package {{STUB_PKG}} --target {{TARGET}}

# Build the Zephyr-stub in release mode
zephyr-stub-release:
    cargo build --package {{STUB_PKG}} --target {{TARGET}} --release

# Build all Phase-4 server binaries (must be done before kernel-phase4)
servers:
    cargo build --package tanix-libsys --package tanix-init \
        --package tanix-pm --package tanix-mem --package tanix-dev \
        --package tanix-worker --target {{TARGET}}

# Build the Phase-5 display stack (display server + Iced-style UI lib + demo)
servers-ui: servers
    cargo build --package tanix-libtanix-ui --package tanix-display \
        --package tanix-ui-demo --target {{TARGET}}

# Build the Phase-4 server binaries + the Phase-7 hog demo
servers-phase7: servers
    cargo build --package tanix-hog --target {{TARGET}}

# Build the Phase-8 window stack (all server binaries up to and incl. hog)
servers-phase8: servers-ui servers-phase7
    cargo build --package tanix-wm --package tanix-counter \
        --package tanix-clock --target {{TARGET}}

# Build the Phase-9 desktop stack (adds ramfs + shell to the Phase-8 set)
servers-phase9: servers-phase8
    cargo build --package tanix-ramfs --package tanix-shell --target {{TARGET}}

# Build the Phase-10 network stack (driver library + net server)
servers-phase10: servers-phase9
    cargo build --package tanix-libdrv --package tanix-net --target {{TARGET}}

# Build the Phase-17 secure-services demo server
servers-phase17: servers-phase10
    cargo build --package tanix-sec --target {{TARGET}}

# Build the kernel with all Phase-9 servers embedded
kernel-phase9: servers-phase9
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features embed-servers

# Build the kernel with all Phase-10 servers embedded
kernel-phase10: servers-phase10
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features embed-servers

# Build the kernel with the Phase-4 server binaries embedded
kernel-phase4: servers
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features embed-servers

# Build the kernel with the Phase-4 servers + Phase-5 display stack embedded
kernel-phase5: servers-ui
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features embed-servers

# Build the kernel with the Phase-5 display stack + Phase-7 scheduler embedded
kernel-phase7: servers-ui servers-phase7
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features embed-servers

# Build the kernel (debug, stub embedded as fallback WFI loop)
kernel:
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}}

# Build the kernel with the real Zephyr-stub embedded
# Run `just zephyr-stub` first to produce the stub binary.
kernel-embed: zephyr-stub
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features embed-zephyr-stub

# Build both in release mode with the stub embedded
release: zephyr-stub-release
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --release --features embed-zephyr-stub

# Build everything in the workspace
all:
    cargo build --target {{TARGET}}

# ── Run ───────────────────────────────────────────────────────────────────────

# Build (debug, fallback stub) and run in QEMU
qemu: kernel
    ./scripts/qemu.sh

# Build with real stub embedded and run in QEMU — Phase 3 VirtIO demo
qemu-phase3: kernel-embed
    ./scripts/qemu.sh

# Build with real stub embedded and run in QEMU — Phase 2 ping-pong demo
qemu-phase2: kernel-embed
    ./scripts/qemu.sh

# Build with Phase-4 servers embedded and run in QEMU — server demo
qemu-phase4: kernel-phase4
    ./scripts/qemu.sh

# Build with the Phase-5 display stack embedded and run in QEMU — UI demo
# (virtio-gpu + virtio-tablet devices; a window shows the UI)
qemu-phase5: kernel-phase5
    ./scripts/qemu.sh -device virtio-gpu-device -device virtio-tablet-device

# Build with the Phase-7 preemptive scheduler and run in QEMU — IRQ-driven
# GPU + tick preemption demo (the hog spins at the lowest priority while
# ticks fire; the display server's virtio-gpu runs interrupt-driven)
qemu-phase7: kernel-phase7
    ./scripts/qemu.sh -device virtio-gpu-device -device virtio-tablet-device

# Build with the Phase-8 window stack and run in QEMU — composited desktop
# demo (window manager + ui-demo/counter/clock apps + hog, all preempted
# by the 1 kHz tick)
qemu-phase8: kernel-phase8
    ./scripts/qemu.sh -device virtio-gpu-device -device virtio-tablet-device

# Build with the Phase-9 desktop stack and run in QEMU — shell demo
# (window manager + ramfs + shell + hog; apps are `exec`'d on demand from
# the shell's keyboard input)
qemu-phase9: kernel-phase9
    ./scripts/qemu.sh -device virtio-gpu-device -device virtio-tablet-device \
        -device virtio-keyboard-device

# Build with the Phase-10 network server embedded and run in QEMU — the
# virtio-net-pci NIC (modern virtio over PCIe, INTx) drives an ARP/ICMP
# ping demo against slirp's 10.0.2.2 gateway
qemu-phase10: kernel-phase10
    ./scripts/qemu.sh -device virtio-gpu-device -device virtio-tablet-device \
        -device virtio-keyboard-device \
        -device virtio-net-pci,netdev=n0,disable-legacy=on \
        -netdev user,id=n0

# Phase-9 keyboard demo: boots the desktop, injects virtio-keyboard events
# over the QEMU monitor and screendumps the shell answering commands
# (screenshots in /tmp/tanix-shell-*.ppm)
qemu-phase9-demo: kernel-phase9
    ./scripts/keyboard-demo.sh

# Phase-8 mouse demo: boots the desktop, injects virtio-tablet pointer
# events over the QEMU monitor and screendumps the cursor following the
# injected path (screenshot in /tmp/tanix-mouse.ppm)
qemu-phase8-mouse: kernel-phase8
    ./scripts/mouse-demo.sh

# Build (release) and run in QEMU
qemu-release: release
    PROFILE=release ./scripts/qemu.sh

# ── Debug ─────────────────────────────────────────────────────────────────────

# Build (debug) and launch QEMU + GDB
debug: kernel
    ./scripts/gdb.sh

# ── Quality ───────────────────────────────────────────────────────────────────

# Run clippy on the kernel (all warnings are errors)
lint:
    cargo clippy --package {{KERNEL_PKG}} --target {{TARGET}} -- -D warnings

# Run clippy on the whole workspace
lint-all:
    cargo clippy --target {{TARGET}} -- -D warnings

# Type-check without producing artifacts
check:
    cargo check --package {{KERNEL_PKG}} --target {{TARGET}}

# ── Housekeeping ──────────────────────────────────────────────────────────────

# Remove build artefacts
clean:
    cargo clean

# Build the Phase-11 socket layer (libtanix-net + net server)
servers-phase11: servers-phase10
    cargo build --package tanix-libnet --package tanix-net --target {{TARGET}}

# Build the kernel with the Phase-11 socket stack embedded
kernel-phase11: servers-phase11
    cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features embed-servers

# Build with the Phase-11 socket stack embedded and run in QEMU — ARP/ping
# probes plus the wire paths: UDP hostfwd in on 5555, UDP markers out on
# 5557, TCP echo listener on 7777, outbound TCP markers to 7778
qemu-phase11: kernel-phase11
    ./scripts/qemu.sh -device virtio-gpu-device -device virtio-tablet-device \
        -device virtio-keyboard-device \
        -device virtio-net-pci,netdev=n0,disable-legacy=on \
        -netdev user,id=n0,hostfwd=udp::5555-:5555,hostfwd=udp::5557-:5557,hostfwd=tcp::7777-:7777,hostfwd=tcp::7778-:7778

# Phase-11 e2e: boots the socket-stack kernel, then drives each wire path
# from the host (UDP in/out, TCP echo, TCP out) and asserts the results
net-test: kernel-phase11
    ./scripts/net-test.sh

# ── Phase 16: sbsa-ref (EL3 monitor + TrustZone) ──────────────────────────────

# Build every server binary *for the sbsa-ref machine*: linked at the 1 TiB
# RAM window (TANIX_LINK_SHIFT), into a separate target dir (target-sbsa)
# so the virt binaries in target/ stay untouched.  The virtio transport
# guest (zephyr-stub) is built here too so the whole embedded image set
# comes from one directory.  Phase 20: the shift also walks the map 32 MiB
# above the raw RAM window to clear the sbsa kernel image (kernel
# `machine_base_shift()` in server.rs must match).
servers-sbsa:
    TANIX_LINK_SHIFT=0xFFC2000000 CARGO_TARGET_DIR=target-sbsa cargo build \
        --package tanix-zephyr-stub --package tanix-libsys \
        --package tanix-init --package tanix-pm --package tanix-mem \
        --package tanix-dev --package tanix-worker \
        --package tanix-libtanix-ui --package tanix-display \
        --package tanix-ui-demo --package tanix-hog --package tanix-wm \
        --package tanix-counter --package tanix-clock --package tanix-ramfs \
        --package tanix-shell --package tanix-libdrv --package tanix-net \
        --package tanix-ping --package tanix-pong --package tanix-sec \
        --target {{TARGET}}

# Build the kernel for sbsa-ref (feature sbsa-ref: EL3-reset boot, EL3
# monitor, secure payload, sbsa linker script) with the sbsa server
# binaries embedded.  Kernel lands in target-sbsa/ too.
kernel-sbsa: servers-sbsa
    CARGO_TARGET_DIR=target-sbsa TANIX_SERVER_TARGET_DIR=target-sbsa \
        cargo build --package {{KERNEL_PKG}} --target {{TARGET}} \
        --features sbsa-ref,embed-servers

# Boot the sbsa-ref kernel in QEMU: EL3 → monitor → EL2 → EL1, TrustZone
# secure payload on the second serial (target-sbsa/sec.log), PSCI from the
# EL3 monitor, phase-16 TCB-measurement + world-switch demos, then the
# machine-agnostic servers (init/pm/mem/dev/worker/net/hog/ping/pong).
qemu-sbsa: kernel-sbsa
    ./scripts/qemu-sbsa.sh
