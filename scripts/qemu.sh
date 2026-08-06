#!/usr/bin/env bash
# Run the Tanix kernel in QEMU (aarch64 virt machine).
#
# Prerequisites:
#   brew install qemu        # macOS
#   apt  install qemu-system-arm  # Debian/Ubuntu
#
# Usage:
#   just qemu                   # debug build, fallback stub
#   just qemu-phase2            # debug build, Zephyr-stub embedded
#   PROFILE=release just qemu   # release build

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-debug}"
KERNEL="${REPO_ROOT}/target/aarch64-unknown-none/${PROFILE}/tanix-kernel"

if [[ ! -f "${KERNEL}" ]]; then
    echo "error: kernel binary not found at ${KERNEL}" >&2
    echo "       run 'just kernel' (or 'just kernel-embed' for Phase 2) first." >&2
    exit 1
fi

echo "Starting QEMU with kernel: ${KERNEL}"
echo "UART output follows (Ctrl-A X to quit QEMU):"
echo "──────────────────────────────────────────────"

exec qemu-system-aarch64 \
    -machine virt,virtualization=on \
    -cpu cortex-a53 \
    -m 256M \
    -nographic \
    -kernel "${KERNEL}" \
    -semihosting-config enable=on,target=native \
    "$@"
