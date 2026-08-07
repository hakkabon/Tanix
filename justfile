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

# Build the kernel with the Phase-4 server binaries embedded
kernel-phase4: servers
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
