#!/usr/bin/env bash
# scripts/check_api_diff.sh
# Public API semver gate. Compares the working tree against the latest
# reachable git tag with `cargo-semver-checks` when the tool is installed.
#
# Exit codes: 0 = clean or SKIP (tool/tag unavailable), 1 = breaking change.

set -euo pipefail

# CI mode: GitHub Actions sets CI=true. A SKIP in a controlled
# environment means the workflow is misconfigured (missing tags or
# tooling) — that must fail the run, not silently skip the gate.
in_ci() { [ -n "${CI:-}" ]; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BASELINE="${1:-}"
if [ -z "${BASELINE}" ]; then
    BASELINE="$(git -C "${REPO_ROOT}" describe --tags --abbrev=0 2>/dev/null || true)"
fi

echo -n "[API] semver diff"
if [ -z "${BASELINE}" ]; then
    if in_ci; then
        echo " FAIL (CI: no git tag baseline — checkout must fetch tags and the repo must be tagged per release)"
        exit 1
    fi
    if [ -f "${REPO_ROOT}/.git/shallow" ]; then
        echo " SKIP (no git tag baseline; shallow clone — run: git fetch --tags --unshallow)"
    else
        echo " SKIP (no git tag baseline)"
    fi
    exit 0
fi

if ! command -v cargo-semver-checks > /dev/null 2>&1 \
    && ! cargo semver-checks --version > /dev/null 2>&1; then
    if in_ci; then
        echo " FAIL (CI: cargo-semver-checks missing — workflow must install it)"
        exit 1
    fi
    echo " SKIP (cargo-semver-checks not installed; cargo install cargo-semver-checks)"
    exit 0
fi

# A baseline pointing at HEAD diffs an empty range: the gate would PASS
# trivially unless the worktree is dirty. Say so instead of passing silently.
if [ "$(git -C "${REPO_ROOT}" rev-parse "${BASELINE}^{commit}" 2>/dev/null || true)" = "$(git -C "${REPO_ROOT}" rev-parse HEAD 2>/dev/null || true)" ]; then
    echo " FAIL (baseline ${BASELINE} is HEAD: committed-but-untagged API changes are invisible to this gate -- tag the release or pass an older baseline)"
    exit 1
fi

CRATE="$(sed -n 's/^name *= *\"\([^\"]*\)\".*/\1/p' "${REPO_ROOT}/Cargo.toml" | head -1)"
echo " baseline=${BASELINE} package=${CRATE}"
cd "${REPO_ROOT}"
# `--package` limits both sides to this crate (one build, not the
# whole workspace). `--release-type auto` lets the crate version (0.6.x
# minor) classify the change; explicit breaking removals still fail.
# `cargo-semver-checks` exposes no `--locked` flag, so a resolve can
# rewrite Cargo.lock behind the gate's back. Snapshot it and fail if the
# tool mutated the lockfile (reproducibility hole the CI gate cannot see).
LOCK_BEFORE="$(git -C "${REPO_ROOT}" hash-object Cargo.lock 2>/dev/null || true)"
set +e
cargo semver-checks check-release --baseline-rev "${BASELINE}" --package "${CRATE}"
SEMVER_RC=$?
set -e
LOCK_AFTER="$(git -C "${REPO_ROOT}" hash-object Cargo.lock 2>/dev/null || true)"
if [ "${LOCK_BEFORE}" != "${LOCK_AFTER}" ]; then
    echo " FAIL (cargo-semver-checks rewrote Cargo.lock; restore it and pin the toolchain)"
    exit 1
fi
exit "${SEMVER_RC}"
