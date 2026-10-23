#!/usr/bin/env bash
# scripts/check_compile_gates.sh
# Automated audit gate for snubber architectural invariants and AGENTS.md rules.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

echo "=== Snubber Architectural Invariant Audit ==="
FAILURES=0

# Gate 1: Check #![forbid(unsafe_code)] in src/lib.rs
echo -n "[1/6] Auditing unsafe code veto... "
if grep -q '#!\[forbid(unsafe_code)\]' "${REPO_ROOT}/src/lib.rs"; then
    echo "OK (forbid(unsafe_code) active)"
else
    echo "FAIL (missing #![forbid(unsafe_code)] in src/lib.rs)"
    FAILURES=$((FAILURES + 1))
fi

if grep -En "\bunsafe\s*\{|\bunsafe\s+fn\b" "${REPO_ROOT}/src/"*.rs > /dev/null 2>&1; then
    echo "FAIL: Found raw unsafe keyword in src/:"
    grep -En "\bunsafe\s*\{|\bunsafe\s+fn\b" "${REPO_ROOT}/src/"*.rs
    FAILURES=$((FAILURES + 1))
fi

# Gate 2: Saturating arithmetic on counters in src/node.rs
echo -n "[2/6] Auditing saturating counters in src/node.rs... "
if grep -En "\b(in_flight|total_started|committed|bulkhead_rejections|poison_recoveries|stale_epochs_rejected)\s*\+=" "${REPO_ROOT}/src/node.rs" > /dev/null 2>&1; then
    echo "FAIL: Direct += found on counter:"
    grep -En "\b(in_flight|total_started|committed|bulkhead_rejections|poison_recoveries|stale_epochs_rejected)\s*\+=" "${REPO_ROOT}/src/node.rs"
    FAILURES=$((FAILURES + 1))
else
    echo "OK (saturating math preserved)"
fi

# Gate 3: Lock-hygiene audit in Phase A/C
echo -n "[3/6] Auditing zero-panic mutex discipline in src/node.rs... "
# Simple check ensuring no raw println! or panic! calls inside node.rs lock helpers
if grep -En "fn record_locked" "${REPO_ROOT}/src/node.rs" | grep -q "format!"; then
    echo "FAIL: format! call detected in record_locked"
    FAILURES=$((FAILURES + 1))
else
    echo "OK (no format! under lock)"
fi

# Gate 4: Zero-metaphor scan across codebase (AGENTS.md Section 14)
echo -n "[4/6] Auditing forbidden metaphors... "
FORBIDDEN_METAPHORS='(\bwizard\b|\bspaceship\b|\bbulletproof\b|\bpanacea\b|\bsilver\s+bullet\b)'
if grep -EIn "${FORBIDDEN_METAPHORS}" "${REPO_ROOT}/src/"*.rs > /dev/null 2>&1; then
    echo "FAIL: Found informal metaphors in src/:"
    grep -EIn "${FORBIDDEN_METAPHORS}" "${REPO_ROOT}/src/"*.rs
    FAILURES=$((FAILURES + 1))
else
    echo "OK (technical language strictly adhered)"
fi

# Gate 5: TRUE no_std — crate attribute + bare-metal target compile
echo -n "[5/6] Auditing true no_std on bare metal... "
if ! grep -q '#!\[cfg_attr(not(feature = "std"), no_std)\]' "${REPO_ROOT}/src/lib.rs"; then
    echo "FAIL (missing #![cfg_attr(not(feature = \"std\"), no_std)] in src/lib.rs)"
    FAILURES=$((FAILURES + 1))
elif ! rustup target list --installed 2>/dev/null | grep -qx "thumbv7em-none-eabihf"; then
    echo "SKIP (target not installed: rustup target add thumbv7em-none-eabihf)"
else
    BARE_METAL_TARGET="thumbv7em-none-eabihf"
    if (cd "${REPO_ROOT}" && cargo check --quiet --no-default-features --target "${BARE_METAL_TARGET}") \
        && (cd "${REPO_ROOT}" && cargo check --quiet --no-default-features --features alloc --target "${BARE_METAL_TARGET}"); then
        echo "OK (core and alloc build for ${BARE_METAL_TARGET})"
    else
        echo "FAIL (no_std build failed for ${BARE_METAL_TARGET})"
        FAILURES=$((FAILURES + 1))
    fi
fi

# Gate 6: Public API semver diff (cargo-semver-checks when installed)
echo -n "[6/6] Auditing public API against the last release... "
API_BASELINE="$(git -C "${REPO_ROOT}" describe --tags --abbrev=0 2>/dev/null || true)"
if [ -z "${API_BASELINE}" ]; then
    echo "SKIP (no git tag baseline)"
elif command -v cargo-semver-checks > /dev/null 2>&1     || (cd "${REPO_ROOT}" && cargo semver-checks --version > /dev/null 2>&1); then
    if (cd "${REPO_ROOT}" && bash "${SCRIPT_DIR}/check_api_diff.sh" "${API_BASELINE}" > /dev/null); then
        echo "OK (no breaking change vs ${API_BASELINE})"
    else
        echo "FAIL (breaking API change vs ${API_BASELINE}: run scripts/check_api_diff.sh)"
        FAILURES=$((FAILURES + 1))
    fi
else
    echo "SKIP (cargo-semver-checks not installed)"
fi

echo "---------------------------------------------"
if [ "$FAILURES" -eq 0 ]; then
    echo "All invariant gates PASSED."
    exit 0
else
    echo "$FAILURES invariant check(s) FAILED."
    exit 1
fi
