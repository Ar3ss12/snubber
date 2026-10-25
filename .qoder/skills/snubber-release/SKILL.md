---
name: snubber-release
description: Cut a snubber release safely - run the full gate sequence, tag, publish, and sync docs. Use when the owner orders a tag or crates.io publish, or asks whether the tree is releasable.
---

# snubber release

One checklist for cutting `snubber` releases. Everything runs through
`scripts/release.sh` (or `just release`), so a human, CI and this skill
execute the same commands.

## Hard rules

1. **Tag and publish only on an explicit owner order.** A green tree is not
   an order. Both actions are public and irreversible enough that they are
   ordered separately (publish can be yanked, never deleted).
2. **Version lives only in `Cargo.toml`.** `Cargo.lock` follows `cargo`.
   Docs quote the version, never redefine it.
3. **Fold `## Unreleased` into the version section before tagging.** A
   published crate and its CHANGELOG must describe the same code.
4. **Never rewrite history, never force-push.** Revert with a new commit.
5. **Local-only docs never ship or gate:** `AGENTS.md`, `tech.md`,
   `docs/DECISION_MATRIX.md`, `FUTURE_node_split.md`, `CODEBASE_REPORT.md`,
   `scripts/analyze_codebase_full.py`, `RESEARCH_ECOSYSTEM_BINDINGS.md`.
   Tests must never read them.
6. **`.ps1` files stay ASCII-only** (PowerShell 5.1 parses cp1252; an em
   dash breaks CI).
7. **Small-RAM box:** run cargo with `TEST_JOBS=2` (or `-j 2`). LNK1102 /
   error 1455 are environmental, not code failures.
8. **Commit and push validated work immediately**, with measured numbers
   (before -> after) for every performance claim. Do not invent numbers; if
   not re-measured, say "historical" plus the date.

## Checklist

1. `git status` clean; every stage in `PLAN.md` closed; working tree matches
   the commit you intend to tag.
2. Update `Cargo.toml` version and `CHANGELOG.md` (fold Unreleased).
3. `TEST_JOBS=2 bash scripts/release.sh --fast` - fmt, clippy, suite +
   doctests + badge, compile gates.
4. Commit the release prep.
5. `TEST_JOBS=2 bash scripts/release.sh` on that exact commit - adds the
   `#[ignore]` release lanes (pool storms, breaker churn, termination),
   the release identity gate and `cargo publish --dry-run`.
6. Only on the owner's explicit order:
   `git tag -a vX.Y.Z -m "snubber X.Y.Z - <theme>"`, `git push origin vX.Y.Z`.
7. Only on the owner's explicit order: `cargo publish`, then verify
   `https://crates.io/api/v1/crates/snubber` reports `max_version=X.Y.Z` and
   `https://docs.rs/snubber/X.Y.Z/snubber/` responds.
8. Sync docs: `PLAN.md` header/journal, `README.md` badge provenance date,
   local `AGENTS.md` / `tech.md` numbers (never committed). Commit and push.

## Failure playbook

- **Release-only test failure** (passes in debug, fails under
  `--release --ignored`): reproduce with
  `cargo test --release --locked --test <suite> -- --ignored --nocapture`,
  print the stats, and fix the threshold to be mechanism-scaled (see
  `docs/INVARIANTS.md`, "Test threshold convention"). Do not lower a floor
  to whatever the run produced.
- **Miri failure with no repository change:** check the pinned nightly in
  `.github/workflows/ci.yml` (`nightly-YYYY-MM-DD`). Bump deliberately in
  its own commit; never revert to a floating `@nightly`.
- **Gate 6 SKIP locally:** expected without `cargo-semver-checks`; CI
  installs it and treats SKIP as FAIL. `release.sh` tolerates only that
  exact SKIP.
- **`cargo publish` refuses:** dirty tree (commit first) or the version
  already exists on crates.io (a published version cannot be reused - bump).
