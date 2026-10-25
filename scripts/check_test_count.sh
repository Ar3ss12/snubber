#!/usr/bin/env bash
# Badge drift gate: the "tests-N passed" badge in README.md must equal the
# number of tests that actually pass in this tree.
#
# Usage:
#   bash scripts/check_test_count.sh [logfile]
#
# With no argument the full suite runs; with a logfile the already captured
# `cargo test` output is parsed instead (CI reuses target/test-suite.log).
#
# Exit codes: 0 match, 1 mismatch/parse error, 2 the test run failed.

set -u

README="${README_PATH:-README.md}"
LOG="${1:-}"

if [ ! -f "$README" ]; then
    echo "FAIL: README not found: $README"
    exit 1
fi

badge="$(sed -n 's/.*tests-\([0-9][0-9]*\)%20passed.*/\1/p' "$README" | head -n 1)"
if [ -z "$badge" ]; then
    echo "FAIL: no 'tests-N%20passed' badge found in $README"
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
        echo "FAIL: cargo test exited $status; badge cannot be validated"
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

# A positive floor turns parse regressions and empty runs into failures.
if [ "$passed" -lt 300 ]; then
    echo "FAIL: only $passed passed tests parsed from $runs result lines (floor 300)"
    exit 1
fi

if [ "$failed" -ne 0 ]; then
    echo "FAIL: $failed test(s) failed; badge must not be validated"
    exit 1
fi

echo "badge=$badge passed=$passed ignored=$ignored runs=$runs"

if [ "$passed" -ne "$badge" ]; then
    echo "FAIL: README badge says $badge passed, tree reports $passed"
    exit 1
fi

echo "OK: badge matches the tree"
