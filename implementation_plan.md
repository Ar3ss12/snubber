# Implementation Plan — Selective Pass-Through (`passthrough`) + Canary Differential

## Overview

Goal: add a build-sealed selective pass-through mode to `snubber` for honest canary comparison (naked run leaks, guarded run rescues), with zero cost to the guarded hot path and a sterile domain-neutral core.

Scope: `SnubberNodeBuilder::passthrough(bool)` sealed at `build()`; lock-free `passthrough_admissions` accounting in `SnubberStats` only (wire `SnapshotV1` 512 B untouched); dummy `TrackedGuard`/`ErasedGuard` that never run rollback and never touch slots/breaker/`EpochGate`; canonical example `13_canary_differential` plus integration test `passthrough_canary`; docs and gates sync. `SnapshotV1`/black-box already exists — no wire change. Zero new dependencies, MSRV 1.70, `#![forbid(unsafe_code)]` kept.

Approach: keep `NodeInner`/PHASE A-C untouched for guarded admissions; branch to the passthrough fast-path at the top of the single admission funnel (`enter_inner`/`enter_armed_with_priority`) behind an immutable `bool` stored outside the mutex; count via outer `Arc<AtomicU64>` (`Relaxed`); dummy guards carry `is_passthrough` and no-op settle; all `enter*`/`scoped*`/`transition*`/async funnels inherit the branch from the funnel, no per-method `if`.

## Types

- `SnubberNodeBuilder::passthrough: bool` (new private field, default `false`): sealed into the node at `build()`. Builder method `pub fn passthrough(mut self, on: bool) -> Self` (`#[must_use]`), documented as immutable perimeter law: no runtime toggle, build two nodes for comparison.
- `SnubberNode { inner: Arc<Mutex<NodeInner>>, passthrough: bool, passthrough_admissions: Arc<AtomicU64> }` (modified struct `src/node.rs:1197`): the flag lives OUTSIDE the mutex (no lock on transit); the counter lives OUTSIDE the mutex (lock-free `fetch_add(Relaxed)`). `Clone` shares both `Arc`s (all handles agree on the law).
- `SnubberStats::passthrough_admissions: u64` (new pub field `src/node.rs:149-204`, after `poison_recoveries`, before `last_fault`): plain counter, filled by `stats()`/`snapshot_stats_no_alloc()` from the atomic `load(Relaxed)`. NOT added to `SnapshotV1`, NOT added to `admission_total()`/`shed_total()` (guarded conservation `attempts == started + rejections` unchanged; passthrough admissions are a separate column).
- `TrackedGuard<F> { is_passthrough: bool }` (new private field `src/node.rs:3454`): dummy guards set `true`, real guards `false`. `commit()`/`abort()`/`Drop`/`settle_implicit`/`check_timeout` early-return no-op when `true` (never run rollback, never touch slots/breaker/wakers).
- `ErasedGuard` passthrough dummy: no struct change. Passthrough path returns `ErasedGuard::new(|| {})` (empty rollback) and counts the atomic; `commit()` disarms (no-op), `Drop` runs the empty closure (no-op) — hardware stays dirty by construction.
- `NodeInner`: NO new fields (keeps PHASE A/C layout, lock discipline, `hard_clamp_slots` untouched). Passthrough never takes the mutex.

## Files

- NEW `G:\forder\snubber\tests\passthrough_canary.rs`: integration canary on abstract atomics (no Mouse/GUI words): passthrough node leaks + counts `passthrough_admissions`, guarded node rescues + keeps guarded books clean; `scoped` and `enter_erased_auto` dummy paths; `SnapshotV1` still 512 B / CRC ok next to passthrough traffic.
- NEW `G:\forder\snubber\examples\13_canary_differential.rs`: canonical Pattern 13 — `VirtualDriver` (`AtomicBool` held flag, domain-neutral name, zero `SendInput`/Win32), cycle 1 naked (`passthrough(true)` leak `HELD=true`), cycle 2 guarded (rescue `HELD=false`), banner + `cargo run --example 13_canary_differential` doc line; guard `Drop` performs the real reset (AGENTS.md §15: no fake `println!` rollback).
- MODIFY `G:\forder\snubber\src\node.rs`: builder flag + build seal; `SnubberNode` two new outer fields; funnel branch; dummy guard constructors; `is_passthrough` no-op settles; stats wiring (`stats()`, `snapshot_stats_no_alloc()`); `is_passthrough()`/`passthrough_admissions()` observers. `NodeInner`, PHASE A/C, `hard_clamp_slots`, `SnapshotV1` untouched.
- MODIFY `G:\forder\snubber\src\lib.rs`: re-export nothing new (all types already exported); only doc-link touch if `passthrough` needs intra-doc link (else untouched).
- MODIFY `G:\forder\snubber\tests\bench_hygiene.rs`: no change needed (no new `bench_function`); only touch if a canary bench is added (not planned).
- MODIFY `G:\forder\snubber\README.md`: one bullet in Architecture/Guarantees + example list entry `13_canary_differential` (keep badge numbers in sync via `tests/doc_sync.rs` tripwire).
- MODIFY `G:\forder\snubber\docs\USER_GUIDE.md`: Layer 2 section — `passthrough` law (builder-sealed, per-node, tests-only/canary; never flip live perimeter), counter semantics, `EpochGate` untouched, conservation note.
- MODIFY `G:\forder\snubber\AGENTS.md`: §0 totals move (new tests count); §3a bench note untouched; new rule line under §9 owner table (`passthrough` flag: builder seals, funnel reads; no runtime toggle) + §15 example index `01..13`.
- MODIFY `G:\forder\snubber\tech.md` (internal, gitignored — local only): module map row touch for `passthrough`, changelog entry.
- DELETE/MOVE: none. No renames (AGENTS.md §16 audit: `13_` is a fresh index, no collision).

## Functions

- NEW `SnubberNodeBuilder::passthrough(mut self, on: bool) -> Self` (`src/node.rs`, next to `trip_after_failures`): `#[must_use]`; stores the law; docs state immutability + two-node comparison pattern.
- NEW `SnubberNode::is_passthrough(&self) -> bool` (`src/node.rs`): `Copy` read of the outer flag, no lock; observer for tests/examples/benches.
- NEW `SnubberNode::passthrough_admissions(&self) -> u64` (`src/node.rs`): `load(Relaxed)` observer; cheap enough for asserts, not for hot loop.
- NEW private `SnubberNode::enter_passthrough_dummy<F>(rollback) -> TrackedGuard<F>` (`src/node.rs`): `fetch_add(1, Relaxed)`; builds `TrackedGuard { guard: None, settled: false, is_passthrough: true, ... }` — drops the user rollback WITHOUT running it via plain `let _ = rollback;` (Rust drops an uninvoked `FnOnce` by freeing its captures without executing the body; physical resource stays HELD=true, proving the leak); arming record stamped with caller coords for debuggability but never published.
- MODIFIED `SnubberNode::enter_inner` (`src/node.rs:2380`): first line branch — `if self.passthrough { return Ok(self.enter_passthrough_dummy(rollback)); }`. Guarded path byte-identical below. Same branch covers `enter`, `enter_auto`, `enter_checked*`, `enter_with_priority*`, `enter_with_payload_budget`, `scoped*`, `transition*` (all funnel through `enter_inner`/`enter_armed*`).
- MODIFIED `SnubberNode::enter_armed_with_priority` (`src/node.rs:2430`): same top branch BEFORE `self.lock()` (never take the mutex on transit). Single choke point — no per-method `if`.
- MODIFIED `SnubberNode::enter_erased` / `enter_erased_auto` (`src/node.rs:1840/2176`): top branch — count atomic + `Ok(ErasedGuard::new(|| {}))`. No probe guard, no slot, no `EpochGate`.
- MODIFIED `SnubberNode::enter_async*` + `try_poll_single` waiter path (`src/node.rs:2797` + `src/async_engine/*`): top branch returns ready dummy (counts atomic); waiters queue never touched in passthrough (no `Waker` parking — transit never pends).
- MODIFIED `TrackedGuard::commit` (`src/node.rs:3685`), `abort` (`:3850`), `settle_implicit` (`:3895`), `check_timeout` (guard: return `WithinBudget` when passthrough), `Drop` (`:3951`): `if self.is_passthrough { /* commit/abort: mark settled, return terminal event, no books */ return; }`. Drop: `if passthrough { self.settled = true; return; }` — user rollback already dropped un-invoked at construction.
- MODIFIED `SnubberNode::stats` (`:1413`) + `snapshot_stats_no_alloc` (`:3104`): fill `passthrough_admissions` from atomic `load(Relaxed)`; `admission_total()`/`shed_total()` unchanged.
- REMOVED: none. No API removed; default `passthrough(false)` keeps every existing call path identical.

## Classes

- MODIFIED `SnubberNodeBuilder` (`src/node.rs:443`): +1 `bool` field + `passthrough()` setter. No trait changes.
- MODIFIED `SnubberNode` (`src/node.rs:1197`): +`passthrough: bool` +`passthrough_admissions: Arc<AtomicU64>` (both outside `Mutex`). `Clone` shares both (derived `Clone` still correct — `Arc` clone + `bool` copy).
- MODIFIED `TrackedGuard<F>` (`src/node.rs:3454`): +`is_passthrough: bool` private field; all constructors updated (real paths `false`, dummy `true`). No public field, no layout promise.
- MODIFIED `SnubberStats` (`src/node.rs:149`): +`pub passthrough_admissions: u64`. `Default` gives 0; `Clone/PartialEq/Eq` derived still hold. `SnapshotV1::from_stats` deliberately does NOT copy it (wire stays 512 B).
- NEW example-side `VirtualDriver` (`examples/13_canary_differential.rs` only, NOT `src/`): `AtomicBool` held-flag, `press()`/`release()`/`is_held()`; never touches Win32. Test-side uses inline atomics, no shared helper crate.
- REMOVED: none.

## Dependencies

- New packages: none. Zero-dep law kept (`AtomicU64`/`Arc` are `std`, already imported in `node.rs:43-45`).
- Version bump `0.5.0` → `0.5.1` (patch: additive, fully backward-compatible — new builder method + new stats field + new test/example files; no signature changed, no behavior changed for default `passthrough(false)`): `Cargo.toml:3` (`version = "0.5.1"`), `Cargo.lock` (`snubber` entry, via `cargo check` regen), README quickstart pin `snubber = "0.5"` stays valid (caret covers 0.5.1), `tech.md` header `snubber v0.5.1` + changelog entry, `docs/USER_GUIDE.md` hang-layer line untouched (still v0.5 family). `0.5.0` stays published on crates.io (immutable, yank NOT needed); `0.5.1` publishes separately on release command.
- Integration: `cargo publish --dry-run` must stay green (no new files outside package `include` — `examples/13_*` + `tests/passthrough_canary.rs` are auto-included like siblings).

## Testing

- NEW `tests/passthrough_canary.rs` (hard asserts, no sleeps):
  1. `naked_run_leaks_and_counts`: passthrough node × N `scoped` with panic work → foreign `AtomicUsize HELD == N` (dirty), `stats().passthrough_admissions == N`, `total_started == 0`, `implicit_drops == 0`, breaker `Closed`, slots return (trivially — never taken).
  2. `guarded_run_rescues`: guarded node × N same work → `HELD == 0`, `passthrough_admissions == 0`, `implicit_drops == N`, conservation holds.
  3. `erased_dummy_never_rolls_back`: `enter_erased_auto` on passthrough → `Drop` without `commit` leaves HELD dirty; guarded twin cleans.
  4. `no_gate_no_slots_touched`: passthrough admissions never bump `stale_epochs_rejected`/`bulkhead_rejections`, `in_flight` stays 0 via `bulkhead_has_room()`.
  5. `wire_stays_512`: `snapshot_v1().encode()` len 512 + `decode` round-trip ok after passthrough traffic (proves the counter never leaked into the frame).
  6. Rollbacks are REAL `fn`/flag bodies (F5: never `|| {}`), counters use local atomics (F1 conservation shape copied from `priority_preemption.rs` WALL print).
- EXISTING: full suite must stay green (`cargo test`, `cargo test --release --test concurrency_stress`, `cargo check --no-default-features`, `cargo bench --bench admission_bench --no-run`); `bench_hygiene` untouched (no new bench fn); `doc_sync` badge tripwire (`test_readme_badge_mirrors_agent_totals`) pins README badge to AGENTS.md §0 totals — 5 new canary tests move the grand total (~237 → ~242+), so the SAME commit must bump the badge in `README.md` and the §0 line in `AGENTS.md` (plus `tech.md` mirror), or CI goes red.
- Miri: new test is atomic+mutex only — add `cargo miri test --test passthrough_canary` to the CI list (cheap, no threads storm, no `posix_spawn`).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, single-threaded full run on 8 GB host (`-j 1 -- --test-threads=1`).

## Implementation Order

1. `src/node.rs` core: builder field + setter → struct fields + `build()` seal → stats field → funnel branch (`enter_armed_with_priority` top) → dummy constructor (`let _ = rollback;` drop-without-invoke) → guard no-op settles → stats wiring → observers. Compile after each sub-step (`cargo check --benches`).
2. `Cargo.toml:3` bump `0.5.0` → `0.5.1` + `Cargo.lock` regen (`cargo check`); verify `cargo publish --dry-run` green with the new version (no code change needed elsewhere — `snubber = "0.5"` caret still resolves).
3. `tests/passthrough_canary.rs`: red test first (leak + count + rescue + erased + wire-512), then green against step 1.
4. `examples/13_canary_differential.rs`: Pattern 13 per §15 (banner, doc line, real `Drop` reset, `cargo run` verified).
5. Docs sync same commit: `README.md` (bullet + example row + badge totals ~242+), `docs/USER_GUIDE.md` (Layer 2 law), `AGENTS.md` (§0 totals, §9 owner row, §15 `01..13`), `tech.md` local (header `v0.5.1` + changelog).
6. Gates: `fmt` → `clippy -D warnings` → `cargo test` → `no-default-features` → `bench --no-run` → `concurrency_stress` release → Miri new test → `publish --dry-run`.
7. Commit: `feat(node): builder-sealed passthrough canary + differential example/test (v0.5.1)`; push `main`. Tag `v0.5.1` + `cargo publish` only on separate release command (`0.5.0` stays live, no yank).

