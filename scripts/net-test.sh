#!/usr/bin/env bash
# Phase-11 network e2e: boots the socket-stack kernel, then drives each
# wire path from the host and asserts the results.
#
# Guest (tanix-net server)                      Host
# ──────────────────────────────────────────    ─────────────────────────
# UDP 5555  <─ datagram (hostfwd udp::5555)     printf | nc -u
# UDP 5557  ─> "tanix-udp-N" markers            nc -u -l 5557 (capture)
# TCP 7777  <─ echo (hostfwd tcp::7777)         printf | nc (expect echo)
# TCP 7778  ─> "tanix-tcp-N" markers            nc -l 7778 (capture)
#
# Prerequisites: qemu-system-aarch64, nc (netcat).  Guest logs are
# asserted through the serial console log.
#
# Usage: PROFILE=release just net-test

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-debug}"
KERNEL="${REPO_ROOT}/target/aarch64-unknown-none/${PROFILE}/tanix-kernel"
LOG="${TMPDIR:-/tmp}/tanix-net-e2e.log"
UDP_CAP="${TMPDIR:-/tmp}/tanix-net-udp-cap"
TCP_CAP="${TMPDIR:-/tmp}/tanix-net-tcp-cap"

if [[ ! -f "${KERNEL}" ]]; then
    echo "error: kernel binary not found at ${KERNEL}" >&2
    echo "       run 'just kernel-phase11' first." >&2
    exit 1
fi

rm -f "${LOG}" "${UDP_CAP}" "${TCP_CAP}"

PASS=0
FAIL=0

ok()   { echo "PASS: $1"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }

# wait_for <pattern> <seconds>
wait_for() {
    local deadline=$(( $(date +%s) + ${2:-40} ))
    while (( $(date +%s) < deadline )); do
        if grep -q "$1" "${LOG}" 2>/dev/null; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}

echo "Booting kernel: ${KERNEL}"
qemu-system-aarch64 \
    -machine virt,virtualization=on,gic-version=3,highmem=off \
    -cpu cortex-a53 \
    -m 256M \
    -nographic \
    -kernel "${KERNEL}" \
    -semihosting-config enable=on,target=native \
    -device virtio-net-pci,netdev=n0,disable-legacy=on \
    -netdev user,id=n0,hostfwd=udp::5555-:5555,hostfwd=udp::5557-:5557,hostfwd=tcp::7777-:7777,hostfwd=tcp::7778-:7778 \
    >"${LOG}" 2>&1 &
QEMU_PID=$!
trap 'kill ${QEMU_PID} 2>/dev/null || true' EXIT

# ── Boot: ARP probe → gateway resolution ──────────────────────────────────────
if wait_for "net: gateway resolved" 60; then
    ok "gateway resolved"
else
    bad "gateway resolution (see ${LOG})"
    exit 1
fi

# ── UDP out: guest markers to 127.0.0.1:5557 ──────────────────────────────────
nc -u -l 127.0.0.1 5557 >"${UDP_CAP}" 2>/dev/null &
NC_UDP=$!
sleep 9
kill "${NC_UDP}" 2>/dev/null || true
if grep -q 'tanix-udp-' "${UDP_CAP}"; then
    ok "UDP out markers on 5557"
else
    bad "UDP out markers (no datagrams in ${UDP_CAP})"
fi

# ── UDP in: host datagram to guest 5555 ───────────────────────────────────────
printf 'tanix-hello-udp-in\n' | nc -u -w1 127.0.0.1 5555 || true
if wait_for "net: UDP_RX port 5555" 10; then
    ok "UDP in on 5555"
else
    bad "UDP in (guest never logged the datagram)"
fi

# ── TCP echo: host connects to guest 7777, sends, expects the echo ────────────
ECHO_OUT="$(printf 'tanix-hello-tcp\n' | nc -w 6 127.0.0.1 7777 2>/dev/null || true)"
if [[ "${ECHO_OUT}" == *"tanix-hello-tcp"* ]]; then
    ok "TCP echo on 7777"
else
    bad "TCP echo (got '${ECHO_OUT}')"
fi

# ── TCP out: guest connects to 127.0.0.1:7778, sends markers ──────────────────
nc -l 127.0.0.1 7778 >"${TCP_CAP}" 2>/dev/null &
NC_TCP=$!
sleep 40
kill "${NC_TCP}" 2>/dev/null || true
if grep -q 'tanix-tcp-' "${TCP_CAP}"; then
    ok "TCP out markers on 7778"
else
    bad "TCP out markers (no data in ${TCP_CAP})"
fi

echo "──────────────────────────────────────────────"
echo "PASS ${PASS}  FAIL ${FAIL}"
[[ "${FAIL}" -eq 0 ]] || exit 1
exit 0
