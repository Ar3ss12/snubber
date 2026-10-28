#!/usr/bin/env bash
# scripts/check_api_diff.sh
# Public API semver gate. Compares the working tree against the latest
# reachable git tag with `cargo-semver-checks` when the tool is installed.
#
# Exit codes: 0 = clean or SKIP (tool/tag unavailable), 1 = breaking change,
# 2 = gate misconfiguration (no usable baseline, missing tool under CI, lock
# mutation), 3 = the verifier could not run (tool/toolchain error, not a
# verdict). check_compile_gates.{sh,ps1} name the class in Gate 6 instead of
# reporting every non-zero as "breaking API change".

set -euo pipefail

# CI mode: GitHub Actions sets CI=true (or 1). A SKIP in a controlled
# environment means the workflow is misconfigured (missing tags or
# tooling) — that must fail the run, not silently skip the gate.
in_ci() {
    case "$(printf '%s' "${CI:-}" | tr '[:upper:]' '[:lower:]')" in
        true | 1) return 0 ;;
        *) return 1 ;;
    esac
}

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
        exit 2
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
        exit 2
    fi
    echo " SKIP (cargo-semver-checks not installed; cargo install cargo-semver-checks)"
    exit 0
fi

# A baseline pointing at HEAD diffs an empty range: the gate would PASS
# trivially unless the worktree is dirty. The one legitimate case is the
# RELEASE COMMIT itself — the tag was just moved onto HEAD (that is what
# tagging a release means). There the meaningful diff is against the
# PREVIOUS tag; fall back to it instead of failing an honest release.
if [ "$(git -C "${REPO_ROOT}" rev-parse "${BASELINE}^{commit}" 2>/dev/null || true)" = "$(git -C "${REPO_ROOT}" rev-parse HEAD 2>/dev/null || true)" ]; then
    PREV="$(git -C "${REPO_ROOT}" describe --tags --abbrev=0 "${BASELINE}^" 2>/dev/null || true)"
    if [ -n "${PREV}" ] && [ "${PREV}" != "${BASELINE}" ]; then
        echo " note: ${BASELINE} is HEAD (release commit) — diffing against previous tag ${PREV}"
        BASELINE="${PREV}"
    else
        echo " FAIL (baseline ${BASELINE} is HEAD and no previous tag exists: committed-but-untagged API changes are invisible to this gate -- tag the release or pass an older baseline)"
        exit 2
    fi
fi

CRATE="$(sed -n 's/^name *= *\"\([^\"]*\)\".*/\1/p' "${REPO_ROOT}/Cargo.toml" | head -1)"
# `--release-type auto` derives the classification from the version bump,
# and for a pre-1.0 crate a 0.x -> 0.y bump counts as MAJOR: the verifier
# then skips every check (0 of 202) and the gate passes vacuously — which
# is exactly how the 0.7.0 `TrackedGuard` auto-trait break slipped past a
# green local run. Inside 0.x the compatible unit is the minor bump, so
# name it explicitly; 1.0+ keeps the version-derived classification.
CURRENT_VERSION="$(sed -n 's/^version *= *\"\([^\"]*\)\".*/\1/p' "${REPO_ROOT}/Cargo.toml" | head -1)"
RELEASE_TYPE="auto"
case "${CURRENT_VERSION}" in
    0.*) RELEASE_TYPE="minor" ;;
esac
echo " baseline=${BASELINE} package=${CRATE} release-type=${RELEASE_TYPE}"
cd "${REPO_ROOT}"
# `--package` limits both sides to this crate (one build, not the
# whole workspace). `--release-type` is explicit for 0.x (see above).
# `cargo-semver-checks` exposes no `--locked` flag, so a resolve can
# rewrite Cargo.lock behind the gate's back. Snapshot it and fail if the
# tool mutated the lockfile (reproducibility hole the CI gate cannot see).
LOCK_BEFORE="$(git -C "${REPO_ROOT}" hash-object Cargo.lock 2>/dev/null || true)"
# The exit code alone cannot separate "breaking change" from "the verifier
# could not run": 0.48.x exits 1 for both, and only 0.50+ split 100 (lint
# failure) from 101 (tool error). The `--- failure <lint>` report heading is
# printed by both generations, so it decides the legacy mixed case.
SEMVER_LOG="$(mktemp)"
trap 'rm -f "${SEMVER_LOG}"' EXIT
set +e
cargo semver-checks check-release --baseline-rev "${BASELINE}" --package "${CRATE}" --release-type "${RELEASE_TYPE}" 2>&1 | tee "${SEMVER_LOG}"
SEMVER_RC=${PIPESTATUS[0]}
set -e
LOCK_AFTER="$(git -C "${REPO_ROOT}" hash-object Cargo.lock 2>/dev/null || true)"
# An empty hash means git could not read the lockfile: the before/after
# comparison would then be `"" == ""` and every mutation would pass unseen.
if [ -z "${LOCK_BEFORE}" ] || [ -z "${LOCK_AFTER}" ]; then
    echo " FAIL (could not hash Cargo.lock: it is missing or git cannot read it — lock mutation would be invisible)"
    exit 2
fi
if [ "${LOCK_BEFORE}" != "${LOCK_AFTER}" ]; then
    echo " FAIL (cargo-semver-checks rewrote Cargo.lock; restore it and pin the toolchain)"
    exit 2
fi
if [ "${SEMVER_RC}" -eq 0 ]; then
    exit 0
fi
if [ "${SEMVER_RC}" -eq 100 ] || grep -q '^--- failure ' "${SEMVER_LOG}"; then
    echo " verdict: cargo-semver-checks reported a semver violation (exit ${SEMVER_RC})"
    exit 1
fi
echo " FAIL (verifier could not run: cargo-semver-checks exit ${SEMVER_RC} — tool/toolchain error, not an API verdict)"
exit 3
