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

# Build the Phase-8 window stack (all 11 server binaries)
servers-phase8: servers-ui servers-phase7
    cargo build --package tanix-wm --package tanix-counter \
        --package tanix-clock --target {{TARGET}}

# Build the kernel with all Phase-8 servers embedded
kernel-phase8: servers-phase8
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
