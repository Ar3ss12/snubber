#!/usr/bin/env bash
# scripts/check_api_diff.sh
# Public API semver gate. Compares the working tree against the latest
# reachable git tag with `cargo-semver-checks` when the tool is installed.
#
# Exit codes: 0 = clean or SKIP (tool/tag unavailable), 1 = breaking change.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BASELINE="${1:-}"
if [ -z "${BASELINE}" ]; then
    BASELINE="$(git -C "${REPO_ROOT}" describe --tags --abbrev=0 2>/dev/null || true)"
fi

echo -n "[API] semver diff"
if [ -z "${BASELINE}" ]; then
    echo " SKIP (no git tag baseline)"
    exit 0
fi

if ! command -v cargo-semver-checks > /dev/null 2>&1 \
    && ! cargo semver-checks --version > /dev/null 2>&1; then
    echo " SKIP (cargo-semver-checks not installed; cargo install cargo-semver-checks)"
    exit 0
fi

echo " baseline=${BASELINE}"
cd "${REPO_ROOT}"
# `--release-type auto` lets the crate version (0.6.0 minor) classify the
# change; explicit breaking removals still fail the gate.
cargo semver-checks check-release --baseline-rev "${BASELINE}"
