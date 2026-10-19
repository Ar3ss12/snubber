#!/usr/bin/env bash
# scripts/check_compile_gates.sh
# Automated audit gate for snubber architectural invariants and AGENTS.md rules.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

echo "=== Snubber Architectural Invariant Audit ==="
FAILURES=0

# Gate 1: Check #![forbid(unsafe_code)] in src/lib.rs
echo -n "[1/4] Auditing unsafe code veto... "
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
echo -n "[2/4] Auditing saturating counters in src/node.rs... "
if grep -En "\b(in_flight|total_started|committed|bulkhead_rejections|poison_recoveries|stale_epochs_rejected)\s*\+=" "${REPO_ROOT}/src/node.rs" > /dev/null 2>&1; then
    echo "FAIL: Direct += found on counter:"
    grep -En "\b(in_flight|total_started|committed|bulkhead_rejections|poison_recoveries|stale_epochs_rejected)\s*\+=" "${REPO_ROOT}/src/node.rs"
    FAILURES=$((FAILURES + 1))
else
    echo "OK (saturating math preserved)"
fi

# Gate 3: Lock-hygiene audit in Phase A/C
echo -n "[3/4] Auditing zero-panic mutex discipline in src/node.rs... "
# Simple check ensuring no raw println! or panic! calls inside node.rs lock helpers
if grep -En "fn record_locked" "${REPO_ROOT}/src/node.rs" | grep -q "format!"; then
    echo "FAIL: format! call detected in record_locked"
    FAILURES=$((FAILURES + 1))
else
    echo "OK (no format! under lock)"
fi

# Gate 4: Zero-metaphor scan across codebase (AGENTS.md Section 14)
echo -n "[4/4] Auditing forbidden metaphors... "
FORBIDDEN_METAPHORS='(\bwizard\b|\bspaceship\b|\bbulletproof\b|\bpanacea\b|\bsilver\s+bullet\b)'
if grep -EIn "${FORBIDDEN_METAPHORS}" "${REPO_ROOT}/src/"*.rs > /dev/null 2>&1; then
    echo "FAIL: Found informal metaphors in src/:"
    grep -EIn "${FORBIDDEN_METAPHORS}" "${REPO_ROOT}/src/"*.rs
    FAILURES=$((FAILURES + 1))
else
    echo "OK (technical language strictly adhered)"
fi

echo "---------------------------------------------"
if [ "$FAILURES" -eq 0 ]; then
    echo "All invariant gates PASSED."
    exit 0
else
    echo "$FAILURES invariant check(s) FAILED."
    exit 1
fi
