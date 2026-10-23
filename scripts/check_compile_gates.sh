#!/usr/bin/env bash
# scripts/check_compile_gates.sh
# Automated audit gate for snubber architectural invariants and AGENTS.md rules.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# CI mode: GitHub Actions sets CI=true. A controlled environment has no
# excuse for a SKIP — the bare-metal target and the semver tool must be
# provisioned by the workflow, and the tag baseline by the release process.
# A SKIP here is a hole in the audit, not a pass.
in_ci() { [ -n "${CI:-}" ]; }

echo "=== Snubber Architectural Invariant Audit ==="
FAILURES=0
SKIPS=0

# Precondition: every scanned path must exist. A missing file makes grep
# exit 2, which used to be indistinguishable from "no match" and let a
# gate print OK over an empty scan.
for need in src/lib.rs src/node.rs src benches examples tests; do
    if [ ! -e "${REPO_ROOT}/${need}" ]; then
        echo "FAIL: required path ${need} is missing — gate scan would be empty."
        exit 1
    fi
done

# Gate 1: Check #![forbid(unsafe_code)] in src/lib.rs
echo -n "[1/6] Auditing unsafe code veto... "
if grep -q '^#!\[forbid(unsafe_code)\]' "${REPO_ROOT}/src/lib.rs"; then
    echo "OK (forbid(unsafe_code) active)"
else
    echo "FAIL (missing #![forbid(unsafe_code)] in src/lib.rs)"
    FAILURES=$((FAILURES + 1))
fi

if UNSAFE_HITS="$(grep -rEIn '(^|[^_[:alnum:]])unsafe[[:space:]]*(\{|fn)' --include='*.rs' "${REPO_ROOT}/src" 2>&1)"; then
    echo "FAIL: Found raw unsafe keyword in src/:"
    printf '%s\n' "${UNSAFE_HITS}"
    FAILURES=$((FAILURES + 1))
else
    rc=$?
    if [ "$rc" -ne 1 ]; then
        echo "FAIL: unsafe scan could not run (grep exit ${rc}) — treating as a hole:"
        printf '%s\n' "${UNSAFE_HITS}"
        FAILURES=$((FAILURES + 1))
    fi
fi

# Gate 2: Saturating arithmetic on counters in src/node.rs
# (scoped to node.rs on purpose: the fields are private to it — tests,
# benches, and examples define their own unrelated `in_flight`s.)
echo -n "[2/6] Auditing saturating counters in src/node.rs... "
PLUS_EQ_HITS="$(grep -En '(^|[^_[:alnum:]])(in_flight|total_started|committed|bulkhead_rejections|poison_recoveries|stale_epochs_rejected)[[:space:]]*\+=' "${REPO_ROOT}/src/node.rs" 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
    # Doc/comment lines are not code: `committed += 1` in a contract
    # comment must not trip a scan for direct counter writes.
    CODE_HITS="$(printf '%s\n' "${PLUS_EQ_HITS}" | grep -vE '^[0-9]+:[[:space:]]*//' || true)"
    if [ -n "${CODE_HITS}" ]; then
        echo "FAIL: Direct += found on counter:"
        printf '%s\n' "${CODE_HITS}"
        FAILURES=$((FAILURES + 1))
    else
        echo "OK (saturating math preserved)"
    fi
elif [ "$rc" -eq 1 ]; then
    echo "OK (saturating math preserved)"
else
    echo "FAIL: saturating-counter scan could not run (grep exit ${rc}):"
    printf '%s\n' "${PLUS_EQ_HITS}"
    FAILURES=$((FAILURES + 1))
fi

# Gate 3: Lock-hygiene audit in Phase A/C
echo -n "[3/6] Auditing zero-panic mutex discipline in src/node.rs... "
# Extract the FULL `fn record_locked` body (signature line to the closing
# brace at the SIGNATURE's own indent — portable POSIX awk: `match` +
# RLENGTH, no gawk `gensub`). The previous fixed `^    \}$` anchor ran to
# EOF if the fn moved one indent deeper (false-FAIL, fails safe but loud).
GATE3_BODY="$(awk '
    match($0, /^[[:space:]]*/) { ind = RLENGTH }
    /^[[:space:]]*fn record_locked\(/ { flag = 1; sig = ind; print; next }
    flag {
        print
        if (ind == sig && $0 ~ /^[[:space:]]*\}[[:space:]]*$/) exit
    }
' "${REPO_ROOT}/src/node.rs")"
if [ -z "${GATE3_BODY}" ] || ! printf '%s\n' "${GATE3_BODY}" | head -1 | grep -q 'fn record_locked('; then
    echo "FAIL: could not extract the fn record_locked body from src/node.rs (renamed or moved?)"
    FAILURES=$((FAILURES + 1))
elif printf '%s\n' "${GATE3_BODY}" | grep -En 'format!|println!|eprintln!|panic!|\.unwrap\(\)|\.expect\(' > /dev/null 2>&1; then
    echo "FAIL: panicking/formatting call inside record_locked:"
    printf '%s\n' "${GATE3_BODY}" | grep -En 'format!|println!|eprintln!|panic!|\.unwrap\(\)|\.expect\('
    FAILURES=$((FAILURES + 1))
else
    echo "OK (no format!/panic! under lock)"
fi

# Gate 4: Zero-metaphor scan across codebase (AGENTS.md Section 14)
# Scope: src + benches + examples + tests — public examples are as
# user-visible as the docs the rule protects. Word boundaries are
# spelled with POSIX char classes only: `\b` is shaky on BSD grep and
# `[[:<:]]` is rejected by GNU grep — `(^|[^_[:alnum:]])...([^_[:alnum:]]|$)`
# works on both.
echo -n "[4/6] Auditing forbidden metaphors... "
FORBIDDEN_METAPHORS='(^|[^_[:alnum:]])(wizard|spaceship|bulletproof|panacea)([^_[:alnum:]]|$)|(^|[^_[:alnum:]])silver[[:space:]]+bullet([^_[:alnum:]]|$)'
if METAPHOR_HITS="$(grep -rEIn "${FORBIDDEN_METAPHORS}" --include='*.rs' "${REPO_ROOT}/src" "${REPO_ROOT}/benches" "${REPO_ROOT}/examples" "${REPO_ROOT}/tests" 2>&1)"; then
    echo "FAIL: Found informal metaphors in src/ benches/ examples/ tests/:"
    printf '%s\n' "${METAPHOR_HITS}"
    FAILURES=$((FAILURES + 1))
else
    rc=$?
    if [ "$rc" -eq 1 ]; then
        echo "OK (technical language strictly adhered)"
    else
        echo "FAIL: metaphor scan could not run (grep exit ${rc}):"
        printf '%s\n' "${METAPHOR_HITS}"
        FAILURES=$((FAILURES + 1))
    fi
fi

# Gate 5: TRUE no_std — crate attribute + bare-metal target compile
echo -n "[5/6] Auditing true no_std on bare metal... "
if ! grep -q '^#!\[cfg_attr(not(feature = "std"), no_std)\]' "${REPO_ROOT}/src/lib.rs"; then
    echo "FAIL (missing #![cfg_attr(not(feature = \"std\"), no_std)] in src/lib.rs)"
    FAILURES=$((FAILURES + 1))
elif ! rustup target list --installed 2>/dev/null | grep -qx "thumbv7em-none-eabihf"; then
    if in_ci; then
        echo "FAIL (CI: bare-metal target missing — workflow must run: rustup target add thumbv7em-none-eabihf)"
        FAILURES=$((FAILURES + 1))
    else
        echo "SKIP (target not installed: rustup target add thumbv7em-none-eabihf)"
        SKIPS=$((SKIPS + 1))
    fi
else
    BARE_METAL_TARGET="thumbv7em-none-eabihf"
    if (cd "${REPO_ROOT}" && cargo check --locked --quiet --no-default-features --target "${BARE_METAL_TARGET}") \
        && (cd "${REPO_ROOT}" && cargo check --locked --quiet --no-default-features --features alloc --target "${BARE_METAL_TARGET}"); then
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
    if in_ci; then
        echo "FAIL (CI: no git tag baseline — checkout must fetch tags and the repo must be tagged per release)"
        FAILURES=$((FAILURES + 1))
    else
        echo "SKIP (no git tag baseline)"
        SKIPS=$((SKIPS + 1))
    fi
elif command -v cargo-semver-checks > /dev/null 2>&1     || (cd "${REPO_ROOT}" && cargo semver-checks --version > /dev/null 2>&1); then
    # No stdout redirect: check_api_diff.sh prints the baseline==HEAD WARN
    # on this same path — swallowing it would hide a reduced audit.
    if (cd "${REPO_ROOT}" && bash "${SCRIPT_DIR}/check_api_diff.sh" "${API_BASELINE}"); then
        echo "OK (no breaking change vs ${API_BASELINE})"
    else
        echo "FAIL (breaking API change vs ${API_BASELINE}: run scripts/check_api_diff.sh)"
        FAILURES=$((FAILURES + 1))
    fi
else
    if in_ci; then
        echo "FAIL (CI: cargo-semver-checks missing — workflow must install it)"
        FAILURES=$((FAILURES + 1))
    else
        echo "SKIP (cargo-semver-checks not installed)"
        SKIPS=$((SKIPS + 1))
    fi
fi

echo "---------------------------------------------"
if [ "$SKIPS" -gt 0 ]; then
    # A skip is a hole, not a pass: reduced coverage must fail the gate.
    echo "$SKIPS gate(s) SKIPPED — coverage reduced, audit NOT complete."
    echo "Re-run with the missing tool available (CI provisions every tool)."
    exit 1
fi
if [ "$FAILURES" -eq 0 ]; then
    echo "All invariant gates PASSED."
    exit 0
else
    echo "$FAILURES invariant check(s) FAILED."
    exit 1
fi
