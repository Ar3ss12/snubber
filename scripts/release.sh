#!/usr/bin/env bash
# scripts/release.sh
# One-shot gate sequence for everything that must be green before a tag is
# cut or a crate is published, ordered so the cheapest failure comes first.
#
# Steps (full run):
#   1. cargo fmt --check
#   2. clippy on all targets/features with -D warnings
#   3. full suite INCLUDING doctests + README badge drift gate (one run)
#   4. architectural compile gates (Gate 6 SKIP is local-only; CI has teeth)
#   5. release ignored lanes: pool storms + breaker churn, termination storm
#   6. release identity gate (Cargo.toml vs latest tag vs CHANGELOG.md)
#   7. cargo publish --dry-run
#
# Usage:
#   bash scripts/release.sh               # full sequence
#   bash scripts/release.sh --fast        # steps 1-4 only (pre-commit check)
#   bash scripts/release.sh --skip-publish # steps 1-6 (post-publish re-verify:
#                                          # a dry run fails once the version
#                                          # is already on crates.io)
#   TEST_JOBS=2 bash scripts/release.sh
#   STRESS_TARGET_KILLS=25 bash scripts/release.sh
#
# Run the full sequence on a CLEAN tree: step 7 refuses a dirty package.
# Exit codes: 0 green, 1 a named step failed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

MODE="full"
case "${1:-}" in
    --fast) MODE="fast" ;;
    --skip-publish) MODE="nopublish" ;;
    "") ;;
    *)
        echo "usage: bash scripts/release.sh [--fast|--skip-publish]" >&2
        exit 2
        ;;
esac

# Plain string (not an array): keeps `set -u` safe on bash 3.2, where an
# empty array expansion is an unbound-variable error.
JOBS=""
if [ -n "${TEST_JOBS:-}" ]; then
    JOBS="-j ${TEST_JOBS}"
fi

LOG_DIR="${REPO_ROOT}/target/release-gate"
mkdir -p "${LOG_DIR}"

case "${MODE}" in
    fast) TOTAL=4 ;;
    nopublish) TOTAL=6 ;;
    *) TOTAL=7 ;;
esac
STEP=0

step() {
    STEP=$((STEP + 1))
    printf '\n== %s/%s %s ==\n' "${STEP}" "${TOTAL}" "$1"
}

die() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}

# The ONLY SKIP line a local release gate may ever print, verbatim.
# Any other "SKIP (...)" means a gate silently did not run, which must
# fail the release: the old condition only verified the PRESENCE of the
# semver SKIP, so a combination like "semver SKIP + Gate 5 SKIP" passed.
audit_gate_skips() {
    local skips bad count
    skips=$(grep -E 'SKIP \(' "${LOG_DIR}/gates.log" || true)
    [ -n "${skips}" ] || return 0
    bad=$(printf '%s\n' "${skips}" |
        grep -v 'SKIP (cargo-semver-checks not installed)' || true)
    if [ -n "${bad}" ]; then
        printf '%s\n' "${bad}" >&2
        die "unexpected SKIP in compile gates; only 'SKIP (cargo-semver-checks not installed)' is tolerated (log: ${LOG_DIR}/gates.log)"
    fi
    count=$(printf '%s\n' "${skips}" | grep -c 'SKIP (cargo-semver-checks not installed)')
    printf 'WARN: %i tolerated gate SKIP line(s): cargo-semver-checks missing locally; CI runs it with teeth.\n' "${count}"
}

# A green `0 passed` on an ignored lane means the lane never ran (a
# renamed test, a cfg'd-out module, a target-filter typo) — a silent
# hole in the release gate. Require executed tests (passed + failed) > 0.
require_executed_tests() {
    local log="$1" lane="$2" executed
    executed=$(awk -F'[ ;]' '/^test result:/{p+=$4; f+=$6} END{print p+f}' "${log}")
    if [ "${executed}" -eq 0 ]; then
        die "ignored lane '${lane}' executed 0 tests (log: ${log})"
    fi
    printf 'lane %s: executed %i test(s)\n' "${lane}" "${executed}"
}

run_ignored_lane() {
    local log="$1"
    shift
    STRESS_TARGET_KILLS="${STRESS_TARGET_KILLS:-50}" \
        cargo test ${JOBS} --release --locked "$@" \
        >"${log}" 2>&1 ||
        {
            tail -n 40 "${log}"
            die "release ignored lane failed (log: ${log})"
        }
    require_executed_tests "${log}" "$*"
}

step "cargo fmt --check"
cargo fmt --check || die "formatting drift (run: cargo fmt)"

step "clippy (-D warnings, all targets/features)"
cargo clippy ${JOBS} --all-targets --all-features -- -D warnings || die "clippy"

step "full suite + doctests + badge"
CARGO_TERM_COLOR=never cargo test ${JOBS} --locked --all-features --color never \
    >"${LOG_DIR}/suite.log" 2>&1 ||
    {
        tail -n 40 "${LOG_DIR}/suite.log"
        die "test suite (log: ${LOG_DIR}/suite.log)"
    }
bash scripts/check_test_count.sh "${LOG_DIR}/suite.log" || die "README badge drift"

step "architectural compile gates"
GATES_EXIT=0
bash scripts/check_compile_gates.sh >"${LOG_DIR}/gates.log" 2>&1 || GATES_EXIT=1
audit_gate_skips
if [ "${GATES_EXIT}" -ne 0 ]; then
    if grep -q 'SKIP (cargo-semver-checks not installed)' "${LOG_DIR}/gates.log"; then
        # The non-zero exit is the documented local-only condition: with
        # cargo-semver-checks missing the audit reports itself NOT
        # complete. audit_gate_skips already proved this semver SKIP is
        # the ONLY skip in the log; CI installs the tool and runs the
        # gate with teeth, so a release can never hide behind this.
        printf 'WARN: compile-gate audit incomplete locally (cargo-semver-checks missing); CI runs it with teeth.\n'
    else
        tail -n 20 "${LOG_DIR}/gates.log"
        die "compile gates (log: ${LOG_DIR}/gates.log)"
    fi
fi
grep -E '^PARITY:' "${LOG_DIR}/gates.log" || true

if [ "${MODE}" = "fast" ]; then
    printf '\nFAST GATES GREEN (lanes, identity and publish dry-run skipped)\n'
    exit 0
fi

step "release ignored lanes"
run_ignored_lane "${LOG_DIR}/lane_storms.log" \
    --test supervisor_pool_stress \
    --test supervisor_pool_stress_lease \
    --test supervisor_pool_stress_mixed \
    --test stress_breaker_churn \
    -- --ignored

run_ignored_lane "${LOG_DIR}/lane_termination.log" \
    --test supervisor_termination \
    -- --ignored

step "release identity (Cargo.toml vs tag vs CHANGELOG)"
bash scripts/check_release_tag.sh || die "release identity"

if [ "${MODE}" = "nopublish" ]; then
    printf '\nRELEASE GATES GREEN (publish dry-run skipped)\n'
    exit 0
fi

step "cargo publish --dry-run"
cargo publish --dry-run || die "publish dry-run (dirty tree or package error)"

printf '\nRELEASE GATES GREEN\n'
printf 'Next, only on an explicit order: git tag -a vX.Y.Z -m "...", push the tag, cargo publish.\n'
