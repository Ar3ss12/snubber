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
echo -n "[1/7] Auditing unsafe code veto... "
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
echo -n "[2/7] Auditing saturating counters in src/node.rs... "
# Every monotonic counter the node books, not just the six that existed
# when this gate was written. A new bucket (rate/breaker/memory/evicted/
# preempted/implicit drops/trips) must not be writable with a bare `+=`.
COUNTERS='in_flight|total_started|committed|explicit_aborts|implicit_drops|bulkhead_rejections|poison_recoveries|stale_epochs_rejected|rate_rejected|breaker_rejections|memory_rejections|evicted|preempted|breaker_trips|reported'
PLUS_EQ_HITS="$(grep -En "(^|[^_[:alnum:]])(${COUNTERS})[[:space:]]*\+=" "${REPO_ROOT}/src/node.rs" 2>&1)"
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
echo -n "[3/7] Auditing zero-panic mutex discipline in src/node.rs... "
# Scan the three functions that run with (or immediately around) the node
# mutex held, not just `record_locked`: a `format!`/`panic!` introduced in
# the admission fast path must fail this gate. Bodies are extracted from
# the signature line to the closing brace at the SIGNATURE's indent
# (portable POSIX awk: `match` + RLENGTH, no gawk `gensub`).
extract_fn() { # $1 = literal signature fragment
    awk -v pat="$1" '
        { match($0, /^[[:space:]]*/); ind = RLENGTH }
        !flag && index($0, pat) > 0 { flag = 1; sig = ind; print; next }
        flag {
            print
            if (ind == sig && $0 ~ /^[[:space:]]*\}[[:space:]]*$/) exit
        }
    ' "${REPO_ROOT}/src/node.rs"
}
GATE3_SYNC="$(extract_fn 'fn enter_inner<')"
GATE3_ASYNC="$(extract_fn 'fn enter_async_attempt<')"
GATE3_RECORD="$(extract_fn 'fn record_locked(')"
GATE3_BAD=""
for body in GATE3_SYNC GATE3_ASYNC GATE3_RECORD; do
    [ -n "${!body}" ] || GATE3_BAD="${GATE3_BAD}${body} (body not found); "
done
# `enter_async_attempt` legitimately uses `expect` for the pre-lock
# `rollback.take()`; everything else must stay non-panicking.
if [ -n "$GATE3_BAD" ]; then
    echo "FAIL: could not extract the lock-path function bodies from src/node.rs: ${GATE3_BAD}(renamed or moved?)"
    FAILURES=$((FAILURES + 1))
elif printf '%s\n%s\n' "$GATE3_SYNC" "$GATE3_RECORD" | grep -En 'format!|println!|eprintln!|panic!|\.unwrap\(\)|\.expect\(' > /dev/null 2>&1; then
    echo "FAIL: panicking/formatting call inside the sync lock path:"
    printf '%s\n%s\n' "$GATE3_SYNC" "$GATE3_RECORD" | grep -En 'format!|println!|eprintln!|panic!|\.unwrap\(\)|\.expect\('
    FAILURES=$((FAILURES + 1))
elif printf '%s\n' "$GATE3_ASYNC" | grep -En 'format!|println!|eprintln!|panic!|\.unwrap\(\)' > /dev/null 2>&1; then
    echo "FAIL: panicking/formatting call inside the async lock path:"
    printf '%s\n' "$GATE3_ASYNC" | grep -En 'format!|println!|eprintln!|panic!|\.unwrap\(\)'
    FAILURES=$((FAILURES + 1))
else
    echo "OK (no format!/panic! in enter_inner, enter_async_attempt, record_locked)"
fi

# Gate 4: Zero-metaphor scan across codebase (AGENTS.md Section 14)
# Scope: src + benches + examples + tests — public examples are as
# user-visible as the docs the rule protects. Word boundaries are
# spelled with POSIX char classes only: `\b` is shaky on BSD grep and
# `[[:<:]]` is rejected by GNU grep — `(^|[^_[:alnum:]])...([^_[:alnum:]]|$)`
# works on both.
echo -n "[4/7] Auditing forbidden metaphors... "
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
echo -n "[5/7] Auditing true no_std on bare metal... "
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
echo -n "[6/7] Auditing public API against the last release... "
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

# Gate 7: Settle paths must drop guards, never forget them.
# `mem::forget(guard)` skips drop glue for EVERY field: on the old
# `TrackedGuard` settle path it stranded one `Arc<Mutex<NodeInner>>` per
# transaction (+24 MB over 10k kills; fixed in 0.5.2, commit e890122). A
# disarmed guard drops cleanly -- its `Drop` sees `settled`/`None` and does
# nothing -- so forgetting buys nothing and silently rots back into that
# leak. Scan all of src/: the offending call lived in the guard impls, not
# only in node.rs helpers.
echo -n "[7/7] Auditing settle paths for mem::forget... "
FORGET_HITS="$(grep -rEn 'mem::forget' --include='*.rs' "${REPO_ROOT}/src" 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
    # Comments may name the historical bug; only code may not call it.
    CODE_HITS="$(printf '%s\n' "${FORGET_HITS}" | grep -vE '^.*:[0-9]+:[[:space:]]*//' || true)"
    if [ -n "${CODE_HITS}" ]; then
        echo "FAIL: mem::forget in src/ bypasses Drop for every field:"
        printf '%s\n' "${CODE_HITS}"
        FAILURES=$((FAILURES + 1))
    else
        echo "OK (settled guards drop; no mem::forget)"
    fi
elif [ "$rc" -eq 1 ]; then
    echo "OK (settled guards drop; no mem::forget)"
else
    echo "FAIL: mem::forget scan could not run (grep exit ${rc}):"
    printf '%s\n' "${FORGET_HITS}"
    FAILURES=$((FAILURES + 1))
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
