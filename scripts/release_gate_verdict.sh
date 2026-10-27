#!/usr/bin/env bash
# Decide whether a compile-gate log allows the release to continue.
#
# Usage: bash scripts/release_gate_verdict.sh <gate-log> <gate-exit-code>
#
# Exit 0 = continue (at most one tolerated WARN), 1 = stop the release.
#
# A real failure is any gate line carrying the `FAIL:` or `FAIL (` token
# (the progress prefix shares the line), or the final
# `N invariant check(s) FAILED.` verdict. Both per-gate forms must be
# matched: when a gate is SKIPPED the script exits before printing the
# failure tally, so `FAIL (missing ...)` plus a tolerated SKIP used to sail
# through as WARN. The only tolerated SKIP is a local missing
# `cargo-semver-checks` (Gate 6); any other SKIP stops the release.
#
# A non-zero gate exit is tolerated only with the gate's own final
# `audit NOT complete.` line: a crash after a SKIP but before the verdict is
# a hole in the audit, not a skip. Conversely, exit 0 with a SKIP line is an
# inconsistent log (reduced coverage must never report as a pass).
set -u

LOG="${1:?usage: release_gate_verdict.sh <gate-log> <gate-exit-code>}"
GATES_EXIT="${2:?usage: release_gate_verdict.sh <gate-log> <gate-exit-code>}"

if [ ! -f "${LOG}" ]; then
    echo "FAIL: gate log not found: ${LOG}" >&2
    exit 1
fi

# Per-gate failures share their line with the `[n/7] ...` progress prefix
# (`[1/7] Auditing unsafe code veto... FAIL (missing ...)`), so anchor on the
# FAIL token, not on column 0. Word boundaries are POSIX char classes for the
# same portability reason as Gate 4 in check_compile_gates.sh.
FAILS="$(grep -E '(^|[^_[:alnum:]])FAIL([:( ]|$)|FAILED\.?$' "${LOG}" || true)"
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
        printf '%s\n' "${SKIPS}" >&2
        echo "FAIL: compile gates exited 0 while reporting a SKIP — reduced coverage is not a pass (log: ${LOG})" >&2
        exit 1
    fi
    if ! grep -q 'audit NOT complete' "${LOG}"; then
        tail -n 20 "${LOG}" >&2 || true
        echo "FAIL: compile gates exited ${GATES_EXIT} after a SKIP without reaching a final verdict (aborted mid-run? log: ${LOG})" >&2
        exit 1
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
