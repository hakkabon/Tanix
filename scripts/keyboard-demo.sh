#!/usr/bin/env bash
# Phase-9 keyboard demo: boots the desktop (wm + ramfs + shell + hog),
# injects virtio-keyboard events over the QEMU monitor and screendumps the
# shell typing commands: `help`, `ls /bin`, `cat /etc/motd`, and
# `exec counter` (which pops the counter app window onto the desktop).
#
# Usage:
#   just qemu-phase9-demo
#   PROFILE=release just qemu-phase9-demo

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-debug}"
KERNEL="${REPO_ROOT}/target/aarch64-unknown-none/${PROFILE}/tanix-kernel"
MON="${TMPDIR:-/tmp}/tanix-kbd-mon"
QEMU_LOG="${TMPDIR:-/tmp}/tanix-kbd-qemu.log"
SHOT_DIR="${TMPDIR:-/tmp}"

if [[ ! -f "${KERNEL}" ]]; then
    echo "error: kernel binary not found at ${KERNEL}" >&2
    echo "       run 'just kernel-phase9' first." >&2
    exit 1
fi

rm -f "${MON}" "${QEMU_LOG}"

echo "Phase-9 keyboard demo — booting desktop (wm + ramfs + shell + hog)..."
qemu-system-aarch64 \
    -machine virt,virtualization=on,gic-version=3 \
    -cpu cortex-a53 \
    -m 256M \
    -nographic \
    -kernel "${KERNEL}" \
    -semihosting-config enable=on,target=native \
    -device virtio-gpu-device -device virtio-tablet-device \
    -device virtio-keyboard-device \
    -monitor "unix:${MON},server,nowait" \
    > "${QEMU_LOG}" 2>&1 &
QPID=$!
trap 'kill ${QPID} 2>/dev/null || true' EXIT

# Wait for the desktop + shell terminal to come up.
for i in $(seq 1 30); do
    if grep -q "shell: window open" "${QEMU_LOG}" 2>/dev/null; then
        break
    fi
    sleep 1
done
sleep 3

python3 - "${MON}" "${SHOT_DIR}" <<'EOF'
import socket
import sys
import time

mon, shots = sys.argv[1], sys.argv[2]

def cmd(s, c, wait=0.2):
    s.sendall(c.encode() + b"\n")
    time.sleep(wait)

def key(s, code, wait=0.1):
    cmd(s, f"input-send-event device virtio-keyboard type 1 code {code} value 1", 0.04)
    cmd(s, f"input-send-event device virtio-keyboard type 1 code {code} value 0", wait)

ENTER = 28

KEYMAP = {}
for ch, code in zip("1234567890-=", [2,3,4,5,6,7,8,9,10,11,12,13]):
    KEYMAP[ch] = code
for ch, code in zip("qwertyuiop[]", [16,17,18,19,20,21,22,23,24,25,26,27]):
    KEYMAP[ch] = code
for ch, code in zip("asdfghjkl;'`", [30,31,32,33,34,35,36,37,38,39,40,41]):
    KEYMAP[ch] = code
for ch, code in zip("zxcvbnm,./", [44,45,46,47,48,49,50,51,52,53]):
    KEYMAP[ch] = code
KEYMAP[" "] = 57
KEYMAP["\\"] = 43

def type_str(s, text):
    for ch in text:
        key(s, KEYMAP[ch])

def enter(s):
    key(s, ENTER, 0.5)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(mon)
time.sleep(0.3)

# 1. `help` — the shell prints its command list.
type_str(s, "help")
enter(s)
time.sleep(0.5)
cmd(s, f"screendump {shots}/tanix-shell-help.ppm")

# 2. `ls /bin` — the embedded app registry.
for ch in "ls /bin":
    key(s, KEYMAP[ch])
enter(s)
time.sleep(0.5)
cmd(s, f"screendump {shots}/tanix-shell-ls.ppm")

# 3. `cat /etc/motd` — read a file over IPC.
for ch in "cat /etc/motd":
    key(s, KEYMAP[ch])
enter(s)
time.sleep(0.5)
cmd(s, f"screendump {shots}/tanix-shell-cat.ppm")

# 4. `exec counter` — spawn a windowed app from the embedded registry.
for ch in "exec counter":
    key(s, KEYMAP[ch])
enter(s)
time.sleep(2)
cmd(s, f"screendump {shots}/tanix-shell-exec.ppm")

s.close()
EOF

echo "keyboard session injected — screenshots in ${SHOT_DIR}/tanix-shell-*.ppm"
kill "${QPID}" 2>/dev/null || true
wait "${QPID}" 2>/dev/null || true
trap - EXIT
