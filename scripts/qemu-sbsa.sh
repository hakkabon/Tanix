#!/usr/bin/env bash
# Run the Tanix kernel on QEMU's sbsa-ref machine — the Phase 16
# "real hardware" story.
#
# The sbsa-ref platform resets its CPUs at EL3 and provides *no* PSCI of
# its own: the platform firmware is expected to supply it.  Tanix's
# kernel *is* the firmware here — its EL3 monitor owns the secure world
# (vectors, PSCI, SMC + SGI dispatchers) and hosts the TrustZone secure
# payload, which prints on the machine's *secure* console.  QEMU routes
# that second PL011 (0x60030000) to serial1; this script sends it to
# target-sbsa/sec.log.
#
# Requires: just kernel-sbsa   (servers + kernel built into target-sbsa/)
#
# Note: QEMU 11 keeps TrustZone on sbsa-ref unconditionally (the `secure`
# machine property was dropped); older QEMU accepted `-machine
# sbsa-ref,secure=on` and behaves the same.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-debug}"
KERNEL="${REPO_ROOT}/target-sbsa/aarch64-unknown-none/${PROFILE}/tanix-kernel"

if [[ ! -f "${KERNEL}" ]]; then
    echo "error: sbsa kernel binary not found at ${KERNEL}" >&2
    echo "       run 'just kernel-sbsa' first." >&2
    exit 1
fi

SEC_LOG="${REPO_ROOT}/target-sbsa/sec.log"
rm -f "${SEC_LOG}"

echo "Starting QEMU (sbsa-ref) with kernel: ${KERNEL}"
echo "NS console follows (Ctrl-A X to quit QEMU); secure console → ${SEC_LOG}"
echo "──────────────────────────────────────────────"

exec qemu-system-aarch64 \
    -machine sbsa-ref \
    -cpu cortex-a57 \
    -m 1G \
    -smp 4 \
    -nographic \
    -serial file:"${SEC_LOG}" \
    -kernel "${KERNEL}" \
    "$@"