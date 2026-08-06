#!/usr/bin/env bash
# Launch QEMU in GDB-server mode, then attach GDB.
#
# Prerequisites:
#   brew install qemu gdb-arm-none-eabi   # macOS (ARM toolchain GDB)
#   apt  install qemu-system-arm gdb-multiarch  # Debian/Ubuntu
#
# Usage:
#   just debug          # debug build

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-debug}"
KERNEL="${REPO_ROOT}/target/aarch64-unknown-none/${PROFILE}/tanix-kernel"

if [[ ! -f "${KERNEL}" ]]; then
    echo "error: kernel binary not found at ${KERNEL}" >&2
    echo "       run 'just kernel' first." >&2
    exit 1
fi

echo "Starting QEMU GDB server on :1234 ..."

# ── Start QEMU in background, halted at reset vector ────────────────────────
qemu-system-aarch64 \
    -machine virt,virtualization=on,gic-version=3 \
    -cpu cortex-a53 \
    -m 256M \
    -nographic \
    -kernel "${KERNEL}" \
    -semihosting-config enable=on,target=native \
    -s -S &                        # -s = listen :1234, -S = freeze at start

QEMU_PID=$!
trap "kill ${QEMU_PID} 2>/dev/null || true" EXIT

# Wait for QEMU to open the port.
sleep 0.5

# ── Attach GDB ───────────────────────────────────────────────────────────────
# Try common AArch64 GDB binary names in order of preference.
GDB=$(command -v aarch64-none-elf-gdb      2>/dev/null \
   || command -v aarch64-linux-gnu-gdb     2>/dev/null \
   || command -v aarch64-elf-gdb           2>/dev/null \
   || command -v gdb-multiarch             2>/dev/null \
   || command -v gdb                       2>/dev/null)

if [[ -z "${GDB}" ]]; then
    echo "error: no suitable GDB found. Install gdb-multiarch or an AArch64 toolchain." >&2
    exit 1
fi

echo "Using GDB: ${GDB}"

"${GDB}" \
    -ex "file ${KERNEL}" \
    -ex "target remote :1234" \
    -ex "set architecture aarch64" \
    -ex "break kmain" \
    -ex "break exception_handler" \
    -ex "continue"
