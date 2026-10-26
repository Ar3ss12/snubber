#!/usr/bin/env bash
# Decide whether a compile-gate log allows the release to continue.
#
# Usage: bash scripts/release_gate_verdict.sh <gate-log> <gate-exit-code>
#
# Exit 0 = continue (possibly with a tolerated WARN), 1 = stop the release.
#
# The only tolerated SKIP is a local missing `cargo-semver-checks`
# (Gate 6 of check_compile_gates). Anything else — a real `FAIL:` line,
# an unexpected SKIP, or a non-zero exit with no explanation — stops the
# release even when the tolerated SKIP is also present. The old inline
# check in release.sh only looked for the SKIP line, so `FAIL` + `SKIP`
# sailed through as WARN.
set -u

LOG="${1:?usage: release_gate_verdict.sh <gate-log> <gate-exit-code>}"
GATES_EXIT="${2:?usage: release_gate_verdict.sh <gate-log> <gate-exit-code>}"

if [ ! -f "${LOG}" ]; then
    echo "FAIL: gate log not found: ${LOG}" >&2
    exit 1
fi

FAILS="$(grep -E '^FAIL:|FAILED\.?$' "${LOG}" || true)"
if [ -n "${FAILS}" ]; then
    printf '%s\n' "${FAILS}" >&2
    echo "FAIL: compile gates reported a real failure (log: ${LOG})" >&2
    exit 1
fi

SKIPS="$(grep -E 'SKIP \(' "${LOG}" || true)"
if [ -n "${SKIPS}" ]; then
    BAD="$(printf '%s\n' "${SKIPS}" | grep -v 'SKIP (cargo-semver-checks not installed)' || true)"
    if [ -n "${BAD}" ]; then
        printf '%s\n' "${BAD}" >&2
        echo "FAIL: unexpected SKIP in compile gates; only 'SKIP (cargo-semver-checks not installed)' is tolerated (log: ${LOG})" >&2
        exit 1
    fi
    if [ "${GATES_EXIT}" -eq 0 ]; then
        echo "OK: compile gates passed (semver tool present)."
        exit 0
    fi
    COUNT="$(printf '%s\n' "${SKIPS}" | grep -c 'SKIP (cargo-semver-checks not installed)')"
    echo "WARN: ${COUNT} tolerated gate SKIP line(s): cargo-semver-checks missing locally; CI runs it with teeth."
    exit 0
fi

if [ "${GATES_EXIT}" -ne 0 ]; then
    tail -n 20 "${LOG}" >&2 || true
    echo "FAIL: compile gates exited ${GATES_EXIT} without a tolerated SKIP (log: ${LOG})" >&2
    exit 1
fi

exit 0
