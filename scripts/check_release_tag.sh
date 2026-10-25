#!/usr/bin/env bash
# scripts/check_release_tag.sh
# Release identity gate: the crate version in Cargo.toml must be consistent
# with the latest git tag and documented in CHANGELOG.md.
#
# Rules:
#   version == tag                       -> OK
#   version  > tag, documented + marked
#     Unreleased in CHANGELOG            -> OK
#   version  > tag, documented + dated   -> WARN (released but not tagged;
#                                           tag only on explicit release order)
#   version  > tag, NOT documented       -> FAIL
#   version  < tag                       -> FAIL (manifest behind the tag)
#   no tag reachable                     -> FAIL in CI, SKIP locally
#
# Tags are never created here: reporting the drift is the gate's job.
#
# Exit codes: 0 = consistent or documented-ahead (WARN), 1 = drift.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

in_ci() {
    case "$(printf '%s' "${CI:-}" | tr '[:upper:]' '[:lower:]')" in
        true | 1) return 0 ;;
        *) return 1 ;;
    esac
}

CARGO_TOML="${REPO_ROOT}/Cargo.toml"
CHANGELOG="${REPO_ROOT}/CHANGELOG.md"

VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "${CARGO_TOML}" | head -1)"
if [ -z "${VERSION}" ]; then
    echo "[RELEASE] FAIL: cannot read version from Cargo.toml"
    exit 1
fi

TAG="$(git -C "${REPO_ROOT}" describe --tags --abbrev=0 2>/dev/null || true)"
if [ -z "${TAG}" ]; then
    if in_ci; then
        echo "[RELEASE] FAIL: no git tag reachable (CI checkout must use fetch-depth: 0)"
        exit 1
    fi
    echo "[RELEASE] SKIP: no git tag in this clone (version ${VERSION} unchecked locally)"
    exit 0
fi

if [ "v${VERSION}" = "${TAG}" ]; then
    echo "[RELEASE] OK: Cargo.toml ${VERSION} matches tag ${TAG}"
    exit 0
fi

TAG_VERSION="${TAG#v}"
if [ "${VERSION}" = "${TAG_VERSION}" ]; then
    echo "[RELEASE] FAIL: tag ${TAG} has a non-canonical form (expected v${VERSION})"
    exit 1
fi

# Manifest behind the tag means users install a version older than the
# release the repository advertises.
LOWEST="$(printf '%s\n%s\n' "${VERSION}" "${TAG_VERSION}" | sort -V | head -1)"
if [ "${LOWEST}" = "${VERSION}" ]; then
    echo "[RELEASE] FAIL: Cargo.toml ${VERSION} is older than tag ${TAG}"
    exit 1
fi

if [ ! -f "${CHANGELOG}" ]; then
    echo "[RELEASE] FAIL: Cargo.toml ${VERSION} is ahead of ${TAG} and CHANGELOG.md is missing"
    exit 1
fi
SECTION_LINE="$(grep -nE "^##[[:space:]]*\[?${VERSION}\]?([[:space:]]|$)" "${CHANGELOG}" | head -1 || true)"
if [ -z "${SECTION_LINE}" ]; then
    echo "[RELEASE] FAIL: Cargo.toml ${VERSION} is ahead of ${TAG} but CHANGELOG.md has no ${VERSION} section"
    exit 1
fi

if grep -Eiq "^##[[:space:]]*(\[unreleased\]|unreleased)" "${CHANGELOG}" \
    || printf '%s\n' "${SECTION_LINE}" | grep -Eiq 'unreleased'; then
    echo "[RELEASE] OK: Cargo.toml ${VERSION} is ahead of ${TAG} and marked Unreleased in CHANGELOG"
    exit 0
fi

echo "[RELEASE] WARN: Cargo.toml ${VERSION} is ahead of the latest tag ${TAG} (documented in CHANGELOG, not tagged)"
echo "          Tag on explicit release order; semver gates diff against ${TAG} meanwhile."
exit 0
