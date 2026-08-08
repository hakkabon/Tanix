#!/usr/bin/env bash
# Phase-8 mouse demo: boots the window stack, injects virtio-tablet pointer
# events over the QEMU monitor, and screendumps the cursor over the desktop.
#
# Usage:
#   just qemu-phase8-mouse
#   PROFILE=release just qemu-phase8-mouse

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-debug}"
KERNEL="${REPO_ROOT}/target/aarch64-unknown-none/${PROFILE}/tanix-kernel"
MON="${TMPDIR:-/tmp}/tanix-mouse-mon"
LOG="${TMPDIR:-/tmp}/tanix-mouse.log"
SCREEN="${TMPDIR:-/tmp}/tanix-mouse.ppm"
QEMU_LOG="${TMPDIR:-/tmp}/tanix-mouse-qemu.log"

if [[ ! -f "${KERNEL}" ]]; then
    echo "error: kernel binary not found at ${KERNEL}" >&2
    echo "       run 'just kernel-phase8' first." >&2
    exit 1
fi

rm -f "${MON}" "${LOG}" "${SCREEN}"

echo "Phase-8 mouse demo — booting window stack..."
qemu-system-aarch64 \
    -machine virt,virtualization=on,gic-version=3 \
    -cpu cortex-a53 \
    -m 256M \
    -nographic \
    -kernel "${KERNEL}" \
    -semihosting-config enable=on,target=native \
    -device virtio-gpu-device -device virtio-tablet-device \
    -monitor "unix:${MON},server,nowait" \
    > "${QEMU_LOG}" 2>&1 &
QPID=$!
trap 'kill ${QPID} 2>/dev/null || true' EXIT

# Wait for the desktop compositor to come up (all three windows).
for i in $(seq 1 30); do
    if grep -q "wm: desktop" "${QEMU_LOG}" 2>/dev/null; then
        break
    fi
    sleep 1
done
sleep 3

python3 - "${MON}" "${SCREEN}" <<'EOF'
import socket
import sys
import time

mon, screen = sys.argv[1], sys.argv[2]

def cmd(s, c, wait=0.4):
    s.sendall(c.encode() + b"\n")
    time.sleep(wait)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(mon)
time.sleep(0.3)

# Trace a diagonal pointer path across the desktop (ABS_X=0, ABS_Y=1).
for step, (x, y) in enumerate(
    [(300, 200), (600, 260), (900, 320), (900, 480), (400, 560), (400, 320)]
):
    cmd(s, f"input-send-event device virtio-tablet type 3 code 0 value {x}")
    cmd(s, f"input-send-event device virtio-tablet type 3 code 1 value {y}")
    cmd(s, "input-send-event device virtio-tablet type 0 code 0 value 0", 0.2)
    if step == 3:
        cmd(s, f"screendump {screen}")
s.close()
EOF

echo "pointer path injected — screenshot: ${SCREEN}"
kill "${QPID}" 2>/dev/null || true
wait "${QPID}" 2>/dev/null || true
trap - EXIT
