#!/usr/bin/env bash
# Suite health gate: the test run that actually happened must be green, the
# parsed counts must clear a floor, and the README must not turn them into a
# public number.
#
# The "tests-N passed" badge was removed on 2026-10-27 (owner's call): a test
# count is a CI-log fact, not a landing-page fact — it duplicates the CI badge,
# goes stale the moment a test is added, and reads as self-praise next to
# anyhow / serde / rayon / tokio. This gate therefore guards the ABSENCE of a
# numeric test claim, which is what actually keeps the contract true.
#
# Usage:
#   bash scripts/check_test_count.sh [logfile]
#
# With no argument the full suite runs; with a logfile the already captured
# `cargo test` output is parsed instead (CI reuses target/test-suite.log).
#
# Exit codes: 0 green + no vanity claim, 1 mismatch/parse error/claim found,
# 2 the test run failed.

set -uo pipefail

README="${README_PATH:-README.md}"
LOG="${1:-}"

if [ ! -f "$README" ]; then
    echo "FAIL: README not found: $README"
    exit 1
fi

# No numeric test-count badge or prose claim may come back: a shields.io
# "tests-N passed" pill, or "<N> passing tests" style sentences.
if grep -Eq 'tests-[0-9]+%20passed|badgen.*tests-[0-9]+|[0-9]+ (passing|passed) tests' "$README"; then
    echo "FAIL: $README publishes a test count again — the number belongs in CI logs:"
    grep -En 'tests-[0-9]+%20passed|badgen.*tests-[0-9]+|[0-9]+ (passing|passed) tests' "$README"
    exit 1
fi

cleanup=""
if [ -z "$LOG" ]; then
    LOG="$(mktemp)"
    cleanup="$LOG"
    trap 'rm -f "$cleanup"' EXIT
    jobs="${TEST_JOBS:-}"
    echo "Running cargo test (log: $LOG) ..."
    if [ -n "$jobs" ]; then
        cargo test --locked --all-features --color never -j "$jobs" >"$LOG" 2>&1
    else
        cargo test --locked --all-features --color never >"$LOG" 2>&1
    fi
    status=$?
    if [ "$status" -ne 0 ]; then
        tail -n 40 "$LOG"
        echo "FAIL: cargo test exited $status; the suite is not green"
        exit 2
    fi
fi

if [ ! -f "$LOG" ]; then
    echo "FAIL: test log not found: $LOG"
    exit 1
fi

# Only "test result:" lines count. `ignored` and `filtered out` are excluded,
# so a suspiciously small sum means the parse broke, not that the suite shrank.
counts="$(awk '
    /^test result:/ {
        passed += $4;
        failed += $6;
        ignored += $8;
        runs += 1
    }
    END { printf "%d %d %d %d\n", passed, failed, ignored, runs }
' "$LOG")"

set -- $counts
passed="$1"
failed="$2"
ignored="$3"
runs="$4"

if [ "$runs" -eq 0 ]; then
    echo "FAIL: no 'test result:' lines in $LOG"
    exit 1
fi

# A positive floor turns parse regressions and empty runs into failures. The
# floor tracks the current suite (467 passed after the 0.6.12 audit fixes)
# instead of the old 300: a `cfg` slip that silently drops a third of the
# tests must fail, not clear a bar set when the suite was smaller.
FLOOR=450
# `cargo test` counts ignored native lanes and ignored doctests. Seven are
# observed (six release/storm lanes plus one ignored doctest); a rising count
# means a test quietly stopped running on the default path.
IGNORED_CEILING=8
if [ "$passed" -lt "$FLOOR" ]; then
    echo "FAIL: only $passed passed tests parsed from $runs result lines (floor $FLOOR)"
    exit 1
fi

if [ "$ignored" -gt "$IGNORED_CEILING" ]; then
    echo "FAIL: $ignored ignored tests (ceiling $IGNORED_CEILING) — a test silently left the default suite"
    exit 1
fi

if [ "$failed" -ne 0 ]; then
    echo "FAIL: $failed test(s) failed"
    exit 1
fi

echo "passed=$passed failed=0 ignored=$ignored runs=$runs"
echo "OK: suite green and the README publishes no test count"
