#!/usr/bin/env bash
# Generic boot-assertion smoke test — Phase 22 (CI harness).
#
# Boots a kernel image in QEMU, waits for one or more log markers to
# appear on the serial console within a timeout, and exits non-zero if
# any marker never shows up. This is the shared primitive behind the CI
# regression jobs (virt, sbsa-ref, SMP, net) and is safe to run locally.
#
# Usage:
#   scripts/boot-test.sh --machine virt --kernel <path> \
#       [--smp N] [--timeout SEC] [--qemu-arg ARG]... \
#       --expect "marker one" [--expect "marker two"]...
#
#   scripts/boot-test.sh --machine sbsa-ref --kernel <path> \
#       [--sec-log <path>] --expect "marker" [--expect-sec "secure marker"]
#
# Exit status: 0 if every --expect / --expect-sec marker appeared before
# the timeout, 1 otherwise. QEMU is always killed on exit.

set -euo pipefail

MACHINE="virt"
KERNEL=""
SMP=""
TIMEOUT=90
SEC_LOG=""
declare -a EXPECT=()
declare -a EXPECT_SEC=()
declare -a QEMU_EXTRA=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --machine) MACHINE="$2"; shift 2 ;;
        --kernel) KERNEL="$2"; shift 2 ;;
        --smp) SMP="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --sec-log) SEC_LOG="$2"; shift 2 ;;
        --expect) EXPECT+=("$2"); shift 2 ;;
        --expect-sec) EXPECT_SEC+=("$2"); shift 2 ;;
        --qemu-arg) QEMU_EXTRA+=("$2"); shift 2 ;;
        *) echo "error: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

if [[ -z "${KERNEL}" || ! -f "${KERNEL}" ]]; then
    echo "error: --kernel <path> is required and must exist (got '${KERNEL}')" >&2
    exit 2
fi
if [[ "${#EXPECT[@]}" -eq 0 && "${#EXPECT_SEC[@]}" -eq 0 ]]; then
    echo "error: at least one --expect or --expect-sec marker is required" >&2
    exit 2
fi

LOG="$(mktemp -t tanix-boot-test.XXXXXX)"
[[ -n "${SEC_LOG}" ]] && rm -f "${SEC_LOG}"

declare -a QEMU_ARGS=()
case "${MACHINE}" in
    virt)
        QEMU_ARGS=(-machine virt,virtualization=on,gic-version=3,highmem=off \
            -cpu cortex-a53 -m 256M -nographic -kernel "${KERNEL}" \
            -semihosting-config enable=on,target=native)
        ;;
    sbsa-ref)
        QEMU_ARGS=(-machine sbsa-ref -cpu cortex-a57 -m 1G -nographic -kernel "${KERNEL}")
        if [[ -n "${SEC_LOG}" ]]; then
            QEMU_ARGS+=(-serial file:"${SEC_LOG}")
        fi
        ;;
    *)
        echo "error: unknown --machine '${MACHINE}' (want virt|sbsa-ref)" >&2
        exit 2
        ;;
esac
[[ -n "${SMP}" ]] && QEMU_ARGS+=(-smp "${SMP}")
QEMU_ARGS+=("${QEMU_EXTRA[@]}")

echo "Booting (${MACHINE}): ${KERNEL}"
echo "  qemu-system-aarch64 ${QEMU_ARGS[*]}"
qemu-system-aarch64 "${QEMU_ARGS[@]}" >"${LOG}" 2>&1 &
QEMU_PID=$!
trap 'kill "${QEMU_PID}" 2>/dev/null || true; wait "${QEMU_PID}" 2>/dev/null || true' EXIT

wait_for_in() {
    local file="$1" pattern="$2" deadline
    deadline=$(( $(date +%s) + TIMEOUT ))
    while (( $(date +%s) < deadline )); do
        [[ -f "${file}" ]] && grep -qF "${pattern}" "${file}" 2>/dev/null && return 0
        kill -0 "${QEMU_PID}" 2>/dev/null || return 1  # QEMU died early
        sleep 0.5
    done
    return 1
}

PASS=0
FAIL=0

for marker in "${EXPECT[@]}"; do
    if wait_for_in "${LOG}" "${marker}"; then
        echo "PASS: console marker '${marker}'"
        PASS=$((PASS + 1))
    else
        echo "FAIL: console marker '${marker}' not seen within ${TIMEOUT}s"
        FAIL=$((FAIL + 1))
    fi
done

for marker in "${EXPECT_SEC[@]}"; do
    if [[ -z "${SEC_LOG}" ]]; then
        echo "FAIL: --expect-sec given but no --sec-log path" >&2
        FAIL=$((FAIL + 1))
        continue
    fi
    if wait_for_in "${SEC_LOG}" "${marker}"; then
        echo "PASS: secure-console marker '${marker}'"
        PASS=$((PASS + 1))
    else
        echo "FAIL: secure-console marker '${marker}' not seen within ${TIMEOUT}s"
        FAIL=$((FAIL + 1))
    fi
done

echo "──────────────────────────────────────────────"
echo "PASS ${PASS}  FAIL ${FAIL}"
if [[ "${FAIL}" -ne 0 ]]; then
    echo "── console log (${LOG}) ──"
    tail -n 80 "${LOG}" || true
    [[ -n "${SEC_LOG}" && -f "${SEC_LOG}" ]] && { echo "── secure log (${SEC_LOG}) ──"; tail -n 40 "${SEC_LOG}"; }
    exit 1
fi
exit 0
