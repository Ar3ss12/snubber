# Changelog

All notable changes to `snubber`. Benchmarks live in [BENCHMARKS.md](BENCHMARKS.md);
the audit-batch fix tables live at the bottom of this file, and the
current-state contracts live in [docs/USER_GUIDE.md](docs/USER_GUIDE.md).

## Unreleased

0.7.1. First, four follow-ups from the 2026-10-29 audit of the 0.7.0
integration: two close a class of drift (a threshold and a label that could
disagree with the code they describe), one publishes a measurement that was
missing, and one moves a detector the consumer had already written twice.
Then the memory-trend batch: the leak detector gains a shape, a rate and a
composed verdict, so a consumer no longer keeps a ring and a level watch
side by side and reconciles them by hand, and the same derivative is
applied to handle counts, which RSS can never show. Then the ordering and
pacing batch: teardown order becomes a type, the concurrency ceiling learns
the sustainable limit instead of a build-time guess, and retries spread
without an RNG. Last, supervision: a child that ignores the soft memory mark
is now retired before the hard wall, with its last verified state handed to
the successor generation.

### Added

- `MemoryTrendVerdict::Leaking { growth_rate_kb_sec }` — a verdict is no
  longer only "over a limit": a full rising window under BOTH limits now
  refuses new admissions with the growth it measured. It is a new type
  rather than a variant of `MemoryVerdict` because that enum has no `repr`:
  a field-carrying variant leaves every discriminant unspecified, which
  `cargo-semver-checks` reports as a major change. `MemoryVerdict` is
  untouched, and `MemoryTrendVerdict::level()` yields it, so a caller that
  only understands levels keeps working.
- `LeakWindow` + `LeakWatch::window()`: one read of the ring — samples,
  full, rising, oldest, newest — without consuming evidence. `growth_kb()`
  lives on the window, so `push` and every reader share one definition of
  "a trend".
- `LeakWatch::growth_rate_kb_sec(interval_ms)` + the free `rate_kb_sec`:
  KiB/s over the span the caller owns, `growth * 1000 / ((N - 1) *
  interval_ms)`, saturating instead of wrapping. `None` (or `0`) when the
  caller supplied no interval or the window is one sample wide — the crate
  never invents a clock.
- `LeakClassification` + `LeakWatch::classify(trip_kb, interval_ms)`:
  `Warming { samples }` / `Stable { growth_kb }` / `Rising { growth_kb,
  rate_kb_sec }` / `Sawtooth`. The last one is the honest gap in the
  monotonic rule — a window that climbs and drops never trips, and now it
  is nameable instead of invisible. Naming a shape is not a verdict; the
  caller decides whether that shape is a fault.
- `MemoryTrend<const N: usize>`: level watch + leak ring as one cold-path
  value with one verdict per sample. The level outranks the trend (`Hard`
  is never relabelled `Leaking`, `Soft` stays `Soft`), and the ring is fed
  even while a level verdict is reported, so a pressure episode does not
  blind the trend. `levels_only` builds the level half with the trend
  disabled. Tests: level priority, warming/stable/rising/sawtooth shapes,
  rate over two intervals, saturation, a dip clearing the rate, and a
  level-only trend that can never report a leak.
- `HandleWatch<const N: usize>` (`handles`, core, `Copy`): the derivative
  rule applied to a count RSS cannot see — handles, file descriptors,
  sockets. A leaked handle is a few bytes of a kernel table, never a
  resident page, so a memory watch is blind to it. The crate still makes no
  OS call: `sample(probe)` takes a caller-supplied `FnMut() -> Option<u64>`,
  and a probe that returns `None` records NOTHING and reports `None` —
  substituting `0` would be a false dip that clears the window and disarms
  the detector on exactly the host whose probe failed. `note(count)` is the
  same step for callers that already hold the number. Tests: a flat
  baseline reads `Sawtooth` and a slow drift `Stable`, a rising window
  names its rate, an unreadable probe leaves the ring untouched, and a dip
  clears the evidence.

- `ExecutionEnvelope::pad_to_floor(wall_us)` + `pad_payload_to_floor(bytes)`:
  raise a measurement to the declared floor and leave anything above it
  untouched, for the caller that must not finish faster than its floor (a
  pacer, a coalescing window) and pads its own wait. The ceiling is not
  touched, so an overrun still reads `Over`, and a `0` floor leaves the value
  as it was. Documented limit: padding the reported duration of work that did
  not happen is the silent-skip signal the floor exists to catch. Tests:
  padding clears the floor verdict, never shortens and never launders an
  overrun, and a disabled floor changes nothing.
- `FlightTape<const N: usize>` + `TapeEntry` (`tape`, core, `Copy`, no
  allocation, no clock): the last `N` `(epoch, task_id, code)` events in a
  fixed array, in the node's `VecDeque` flight-recorder role but usable where
  an allocation on the way to a crash is the first thing to fail (OOM,
  firmware). A full tape overwrites its oldest cell, so it always holds the
  last `N` events in order; `last_with_code` walks newest first; `N = 0`
  records nothing instead of panicking. Tests: short tape in event order,
  overwrite at capacity, `N = 0`, newest-match search direction, and `clear`
  rewinding the cursor.
- `AimdLimit` (`policy`, core, `Copy`, 16 B): additive increase /
  multiplicative decrease for a concurrency ceiling, so the limit converges
  on what the downstream sustains instead of a build-time guess. `on_success`
  adds `step` up to `max`, `on_pressure` keeps `keep_percent` of the ceiling
  and never drops below `min`; the constructor clamps `keep_percent` to
  `1..=99` (0 collapses to `min`, 100 never shrinks — neither is a
  decrease), `max` to at least 1 and `step` to at least 1. Fed one outcome
  per window, never one per admission. Tests: the ceiling, the floor, and a
  broken policy being clamped.
- `jitter_ms(base_ms, spread_percent, salt)` + `Backpressure::jittered(..)`
  (`policy`, core, no RNG, no clock): a symmetric ±`spread_percent` band
  around the node's advice, offset by the SplitMix64 finalizer over a
  caller-supplied salt. Same salt, same delay — deterministic and testable;
  distinct salts spread a herd. `spread_percent == 0` returns the base, and
  `salt == 0` lands on the low edge (documented, not treated as neutral).
  `Backpressure::jittered` moves only `retry_after_ms`; depth and kind stay
  the diagnosis. Tests: determinism, band membership and symmetry, the
  disabled cases, no wrap near `u64::MAX`, and a reading that moves only the
  delay.
- `RankedNode<const RANK: u8>` + `RankedSet<const CAP: usize>` +
  `RankedEntry` (`rank`, `std`): teardown order carried by the type instead
  of by a runtime `rank: u8` argument every call site can get wrong. Higher
  `RANK` stops first, ties keep insertion order, and the set sorts at insert
  (one backward shift in a fixed array) so a stop loop reads front to back
  with no sort and no allocation. A full set refuses with
  `RankedSetError::Full` and keeps its order — evicting would silently drop
  a stop step. The rank becomes data only on admission (`RankedEntry::rank`),
  because one set holds entries of many `RANK` types. Tests: ordering by
  rank rather than registration, tie stability, refusal on a full set, and
  the rank being a type parameter.
- `Rotation` + `RotationPolicy` + `RotationStep` + `RotationHandoff`
  (`supervisor`, `std`, all `Copy`): preventive rotation. `MemoryPolicy`
  says when memory is a problem; this says what to do while there is still
  headroom. The first soft sample opens `grace_ms` in which the child may
  still drain itself and exit on its own path; the deadline is fixed there
  and never extended by later samples, and a sample below soft closes the
  window, so a spike that recovers never rotates. A child still alive at
  the deadline gets a standing `Rotate` order (a missed tick cannot read an
  overdue child as healthy). `handoff` then takes the last verified 64 B
  digest off the retired drain, bumps the generation and counts the
  rotation in one call; `Supervisor::spawn_successor` spawns the successor
  with that digest on stdin — or a NULL stdin (immediate EOF, never a
  block on the inherited terminal, never a hybrid state) when the child was
  killed before publishing a complete three-frame digest. `max_rotations`
  (`0` = unlimited) bounds the churn, and a `Hard` sample is never a
  rotation trigger — that path belongs to `MemoryPolicy::should_kill`.
  Tests: window boundary, no deadline extension, recovery below soft, hard
  path, budget exhaustion, unlimited budget, disabled policy, an unplanned
  death that keeps the budget, digest handoff, torn digest, and two real
  successor processes (digest bit-exact on stdin / immediate EOF).
- `SnubberNode::rollback_budget_ms()`: the budget the node was built
  with, `0` when the clock is unarmed. The observer no longer has to keep
  its own copy of a number the builder owns — that copy is how a watcher
  ends up timing bodies against a budget no node ever armed.
- `watchdog::poll_rollback_node(&node)`: the same read as `poll_rollback`,
  timed against the node's own budget. This is the form a supervisor tick
  should call; the 0.6-shaped `poll_rollback(&node, budget_ms)` stays
  exactly as it was (a public signature is a contract, and the semver gate
  treats an arity change as breaking even under the 0.x minor rule), so
  the threshold can come from the caller but the default can no longer
  drift from the builder.
- `LeakWatch<const N: usize>` + `growth_crosses(growth_kb, trip_kb)` in
  `memory` (core, `Copy`, 96 B at `N = 10` on 64-bit, 88 B on 32-bit): the leak is a DERIVATIVE,
  not a level. `push` returns `Some(newest − oldest)` only when the ring
  is full and every sample beats its predecessor; one sample below its
  predecessor clears the window, because a healthy process oscillates.
  `N` and the trip mark stay caller policy (`N = 0` never reports,
  `trip_kb == 0` disables the verdict). The crate still measures nothing:
  the caller feeds one external sample per tick. Tests: partial window,
  full strictly-rising window, wrap-around recompute, one dip, `N = 0`,
  the trip mark as a growth, and the two composed. Doc-synced into
  `USER_GUIDE.md` §6 and the quick reference; `INVARIANTS.md`
  Invariant 8 states the rule.
- Contended bench row `enter_erased_holding_auto + commit (6 threads)`:
  the holding erased path had no saturation number, so its cost under
  contention was unquoted. Measured 2026-10-29 on the same host and
  settings as the 2026-10-26 table: 1157.9 / 1180.2 ns on two full runs
  (~1158 ns published in `BENCHMARKS.md` and the evidence snapshot).

### Changed

- `watchdog::poll_rollback_node(node)` is the form a supervisor tick
  should call: it reads the node's own budget. `tests/rollback_guardrails.rs`
  pins both directions (unarmed node → `None` even with a caller budget;
  armed node → `budget_ms` equals the builder's), and NanoClick's 1 Hz
  watcher and release latch were migrated to it.
- `tests/doc_sync.rs` now pins bench row LABELS, not only numbers. A
  wrong label (`enter_erased_auto` naming the `probe_erased_auto` row)
  survived the 2026-10-28 doc fix because the exporters carried a third
  copy of every label: the next bench run would have restored it. The
  gate reads the literal closures out of `benches/admission_bench.rs`,
  requires every label to name a function its closure actually calls,
  requires both exporter maps to agree, and requires the artifact and
  `BENCHMARKS.md` cells to start with that label (or the Criterion key).
  It catches wrong NAMES, not imprecise prose.

## 0.7.0 (2026-10-28)

Four seams the first consumer had already worked around in application
code, now typed by the crate: a CPU sampler that can read per-thread
state, a core liveness witness with no OS handle, an opt-in preemption
policy, and an erased admission that can actually hold the ceiling. The
API is additive except for the erased names, which keep their old
spellings as deprecated aliases; core/bare-metal and the zero-dependency
manifest are unchanged.

### Added

- `TrackedGuard::with_cpu_sampler_shared(Arc<dyn Fn() -> u64 + Send + Sync + UnwindSafe + RefUnwindSafe>)`:
  the CPU sampler may now capture state (a thread handle, a worker's own
  counter) instead of being a plain `fn() -> u64`. Same arming law — the
  sample is taken on attach and every verdict measures a delta — and the
  shared sampler wins when both are attached. `with_cpu_sampler(fn)` is
  unchanged and still allocates nothing. `node` is `std`-gated, so the
  `core`/`alloc` lanes never see the field; `TrackedGuard<fn()>` grows
  304 → 320 B (measured, `tests/guard_sampler.rs`), the price of one fat
  pointer. Tests: capture-aware SPIN and SLEEP verdicts, the size ceiling,
  a compile-time assertion that `TrackedGuard` keeps `UnwindSafe` +
  `RefUnwindSafe` (the auto-trait bounds are part of the sampler type — a
  bare `dyn Fn` strips both), and a text tripwire that no unguarded
  `pub use node` enters `src/lib.rs`.
- `sentinel` module (core): `ProcessWitness` / `NoWitness`, `SentinelSample`,
  `SentinelConfig` and `Sentinel`, plus the pure rule `classify_liveness`.
  A supervised process can now report `Alive`, `Silent { idle_ms }` or
  `Dead { armed }` without the crate opening a handle: the OS adapter is
  application code, like `OrphanGuard`. `None` from a witness means
  "cannot observe", never "dead"; the grace comparison is strict
  (`idle_ms == grace` is still `Alive`) and `idle_grace_ms = 0` disables
  `Silent` only. Core-only: no clock, no allocation, no `std`. Unit tests
  run in the `--no-default-features --lib` lane.
- `PipeDrain::armed()` (non-consuming peek at the rescue slot) and
  `PipeDrain::sentinel_sample()` (one `SentinelSample`, `witness_alive`
  is `None`), plus the public `await_post_mortem(&drain, eof_grace_ms)`
  that `run_loop` now calls instead of its inline loop. An application
  with its own supervisor no longer re-derives the EOF-grace rule — and
  can no longer lose the last `RECOVER` token by taking the slot too
  early. `tests/sentinel_pipe.rs` proves the late-token rescue.
- `PreemptPolicy` on the builder: `BackgroundOnly` (default, the Phase 1
  law — only `Critical` knocks, only pure-`Background` occupancy) or
  `StrictSenior` (any lane knocks every strictly junior lane: `Normal`
  over `Background`, `Critical` over `Background` + `Normal`). Both stay
  cooperative: one request, caller retries, victims yield through their
  own conduit. One default-behaviour fix rides along —
  `preempt_requested` now clears when the node fully drains
  (`in_flight == 0`) instead of on the first `Background` settle, so a
  late or unpolled victim cannot silently swallow the request. New
  diagnostics: `SnubberNode::preempt_policy()`. Tests:
  `tests/priority_preemption.rs` (4 added, the three Phase 1 tests
  unchanged).

### Changed

- **Erased admissions split by name.** The probe path is now
  `probe_erased` / `probe_erased_auto`: it releases the bulkhead slot at
  entry, exactly as `enter_erased_auto` always did — same code path, an
  honest name. A slot that is held until settle is a separate entry,
  `enter_erased_holding` / `enter_erased_holding_auto`, so
  `max_in_flight(N)` becomes a real ceiling over LIVE erased
  transactions and `commit()` / `abort()` / `Drop` each release exactly
  one slot and wake one parked `enter_async` waiter. Before → after:

  | 0.6 name | 0.7 name | Behavior |
  |---|---|---|
  | `enter_erased(epoch, task_id, rb)` | `probe_erased(epoch, task_id, rb)` | unchanged: slot released at entry |
  | `enter_erased_auto(rollback)` | `probe_erased_auto(rollback)` | unchanged: slot released at entry |
  | — | `enter_erased_holding(epoch, task_id, rb)` | new: slot held until settle |
  | — | `enter_erased_holding_auto(rollback)` | new: slot held until settle |

  `enter_erased` / `enter_erased_auto` stay as `#[deprecated(since =
  "0.7.0")]` aliases. README gained a "Bulkhead Scope" section,
  `tests/doc_sync.rs` pins `probe_erased_auto` as the canonical target,
  and `tests/erased_holding.rs` (6 tests) pins the holding path:
  ceiling, one release on commit, on drop and on a panicking rollback,
  the probe path unchanged, and conservation. `tests/guard_alloc_budget.rs`
  now storms the holding path too (300k guards per phase, still 0 B net).

## 0.6.13 (2026-10-27)

A patch release of the verification machinery, one teardown ordering and the
docs, two hours after 0.6.12. The public API is unchanged:
`cargo-semver-checks` 0.51.0 against v0.6.12 reports 202 checks pass, 58 skip.

### Fixed

- `scripts/release_gate_verdict.sh` read a `FAIL (...)` line that shared its
  line with the `[n/7]` progress prefix as a non-failure, and accepted a
  tolerated semver SKIP as a complete audit. The scan now matches the `FAIL`
  token anywhere on the line, requires the gate's own final verdict, and
  rejects exit 0 that still carries a SKIP. Ten synthetic logs pin the
  matrix, including `FAIL (` plus a tolerated SKIP -> exit 1.
- `ResumeFuture::drop` removed its waker under the queue mutex and dropped it
  inside the same critical section. Removal stays under the lock (it is the
  dedup boundary); the drop now runs after the guard is released. A probe
  waker whose `Drop` calls `try_lock` proves it: `left: 2` on the old
  `retain`, `1` after the fix.
- `friction_wait_ms` (sync and async twins) computed `depth * cap` in u64:
  `max_wait_ms = u64::MAX` panicked in debug and wrapped in release. Both use
  u128 intermediates and `saturating_add` now; a regression holds a slot at
  `u64::MAX` and asserts the ceiling is returned exactly.
- Gate 6 reported every non-zero verifier exit as "breaking API change". CI
  hit this false verdict for real: cargo-semver-checks 0.48.0 parses rustdoc
  JSON v56/v57 only, stable 1.99 emits v61, so the tool never ran. The gate
  now classes 0 clean / 1 semver violation / 2 gate misconfiguration /
  3 verifier could not run, and `check_compile_gates.{sh,ps1}` names the
  class; a legacy 0.48-style exit 1 is split by the `--- failure <lint>`
  report heading.

### Changed

- `cargo-semver-checks` is pinned to 0.51.0 in both CI install steps (quality
  and Windows parity): it parses rustdoc v57/v60/v61.
- The compile gates print `PARITY` only after a clean failure tally, so a
  failed audit never ends with a complete-looking fingerprint; a tolerated
  local semver SKIP still prints it after the reduced-coverage verdict.
- The suite floor moves 300 -> 450 with an ignored ceiling of 8 (measured
  suite: 467 passed, 7 ignored, 67 result lines).
- CI gains a real 32-bit runtime lane: `frame_fuzz` runs on
  `i686-pc-windows-msvc` (WoW64), reproduced locally; the quality job keeps
  the i686 compile check.
- `benches/admission_bench.rs`: `drain_sync` verification left the timed
  window, while `feed` assertions stay inside it because the bench-hygiene
  gate requires every timed `feed` to assert its parsed frame count.

### Documentation

- `OP_STATE` packaging is documented as 24/24/16 bytes (frames 0/1 carry 24
  digest bytes, frame 2 carries 16), matching `drain.rs` and `supervisor.rs`.
- `SafeLockExt` describes its poison tick as best-effort: skipped while the
  ledger lock is held, under-reporting by design.
- `Protected::abort` keeps a structural guard invariant
  (`expect("protected guard invariant")`) instead of fabricating an empty
  `TransactionHeader`.
- Bench topology statements match the harness: contended rows are 6
  concurrent OS threads with no CPU affinity pinning, and the SPSC bridge is
  a two-thread handoff, not an inter-core bus measurement. `BENCHMARKS.md`,
  `docs/USER_GUIDE.md` and the bench comments say the same thing.

## 0.6.12 (2026-10-27)

Rollback bodies are named, budgeted and observable from outside the node
lock; the false "rollback re-entry deadlocks" law is gone. No existing
public signature changed — the new surface is additive (`#[non_exhaustive]`
enums, new methods, new `RollbackPhase` fields). `SnubberStats` keeps its
0.6.11 shape: it is exhaustively constructible, so the rollback columns
live on `RollbackPhase` instead of growing that struct.

### Fixed

- The User Guide claimed that calling back into the same node from a
  rollback body "deadlocks". Measured on all three paths it does not: the
  ledger lock is released before the body runs. The guide now states the
  measured behavior (sync refuses at capacity, async parks, erased scopes
  are granted), and `tests/doc_sync.rs` fails if the myth returns.
- A slow rollback left no trace (`last_fault: None` after 400 ms). It now
  lands in `RollbackPhase::stalls` / `max_ms` and — when
  the body RETURNS past the budget — in
  `reported[FaultKind::RollbackStall]`, `last_fault` and the flight ring.
  A body that panics mints no verdict: an unwind is not a measurement.
- `tests/lock_hierarchy.rs` now proves what Invariant 6 only asserted:
  every rollback invocation site (13, inventory count asserted) runs with
  the node ledger lock released.

### Added

- `SnubberNode::rollback_phase()`: lock-free view (`active`, `epoch`,
  `task_id`, `elapsed_ms`, `budget_ms`, `max_ms`, `stalls`, `reentries`)
  answerable from any thread while a body runs.
- `SnubberNodeBuilder::rollback_budget_ms(n)`: arms the phase clock. The
  default stays clock-free — two atomic writes per body, no clock read.
- `watchdog::poll_rollback(&node, budget_ms) -> Option<RollbackStall>`:
  a pure read (no lock, thread or allocation) that books nothing. The
  observer supplies the cadence; the node supplies the clock, so an
  unarmed node honestly returns `None` instead of inventing age.
- `RollbackReentrancy::{Allow (default), Refuse}` plus
  `SnubberNodeBuilder::rollback_reentrancy(..)`: `Allow` keeps 0.6.x
  behavior and counts re-entries in `RollbackPhase::reentries`;
  `Refuse` returns `EnterError::RollbackReentrant` before any lock on the
  same thread (sync and async), other threads unaffected.
- `FaultKind::RollbackStall` (14th kind, `Warn`, metric tag
  `rollback_stalls_total`), `StallKind::RollbackStall` and
  `classify_rollback(elapsed_ms, budget_ms)`.
- `examples/20_rollback_budget.rs`: a rollback body that re-locks a held
  mutex, named from outside the lock; the example exits while its worker
  stays wedged.

### Changed

- `SnapshotV1` is v6: the per-kind `reported` tail grew 13 → 14 for
  `RollbackStall`. The 512 B frame contract holds (330/508 B used); older
  frames refuse as `BadVersion` instead of half-parsing.
- `StallKind::RollbackStall` is declared LAST (after `DeadlockStall`), not
  between `CpuSpin` and `DeadlockStall`: inserting it there shifted
  `DeadlockStall` from 3 to 4, which `cargo-semver-checks` reports as
  `enum_no_repr_variant_discriminant_changed`. Appending keeps every
  0.6.11 discriminant (and any caller-side `as` cast) where it was.
- The three rollback counters do NOT join `SnubberStats`: that struct is
  exhaustively constructible in 0.6.11, so any added field — public or
  private — is a `constructible_struct_adds_field` break. `stalls`,
  `max_ms` and the new `reentries` are read from `rollback_phase()`, whose
  shape is free because the type does not exist in 0.6.11.
- Docs carry the rules CI taught us: README's guarantee list names the
  rollback view, the guide's quick-reference table gains `rollback_phase`
  / `rollback_budget_ms` / `rollback_reentrancy` / `poll_rollback` rows
  (layer column pinned by `tests/doc_sync.rs`), and Invariant 12 states
  "append enum variants last" and "never add a field to a constructible
  struct — publish a new `#[non_exhaustive]` view".
- The guide gained a "Host runtimes" section stating the four integration
  laws without metaphors or numbers: a cancelled await books
  `FaultKind::ImplicitDrop` (cancellation is not a clean abort — expected
  timeouts are decided inside the guarded future, and `checkpoint` names
  the stage if one still unwinds), a blocking rollback is budgeted and
  polled from outside, Rayon's rollback runs after the join and the panic
  still reaches the caller, and `SafeLockExt` keeps the same `Mutex` with
  a best-effort counted recovery whose data needs revalidation. A
  doc_sync oracle pins the whole section plus the "does not replace
  `Mutex`" law in README and guide; the negative proof (an extra
  "replaces `Mutex`" line) fails with the cross-doc tripwire.

### Tests

- `tests/rollback_guardrails.rs`: 15 tests plus one `#[ignore]`
  subprocess canary pin the measured re-entry law, the held bulkhead slot,
  the phase view, the budget verdict, the re-entrancy policy and the
  outside watchdog. The canary now requires the child to name its own
  `RollbackStall` before the parent kills it. The taxonomy test also pins
  the 0.6.11 `StallKind` values (`0..=3`) as a local oracle for
  `enum_no_repr_variant_discriminant_changed`: moving `RollbackStall` back
  between `CpuSpin` and `DeadlockStall` fails it with `[0, 1, 2, 4, 3]`
  (injected, observed, reverted byte-for-byte).
- Suite 446 → 465 passed (67 result lines); `scripts/release.sh` runs the
  rollback canary as an ignored release lane, and a lane that executes
  0 tests fails the release.

## 0.6.11 (2026-10-27)

Audit-3 closeout (PLAN_AUDIT3 findings K1/K2, H1–H7, D1–D4; D5 was
dismissed after re-checking the examples). No API changes; the public
surface, wire format and telemetry layout are untouched.

### Fixed

- Panic-macro discipline is a compiler oracle now: `src/lib.rs` carries
  `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`, so
  an `unwrap`/`expect`/`panic!` in ANY library function — including
  helpers called while the node mutex is held — fails the clippy gate.
  The old Gate 3 text scan saw only three function bodies; its scans for
  those macros were removed (the remaining Gate 3 scan keeps the
  allocation/formatting half — `format!`/`print*` under the lock — which
  has no stable lint). The seven legitimate `expect` sites (protected
  resource invariant, pre-lock rollback take, async single-settle) carry
  `#[allow(clippy::expect_used)]` with their reason; the eight internal
  test modules allow the panics they deliberately exercise. MSRV 1.70
  library lanes (`cargo check --lib`, core/alloc/std) unaffected.

- Gate 2's counter scan scope is derived at run time from `pub struct
  SnubberStats` (every `pub u64` / `pub [u64; N]` field) plus `in_flight`,
  in both the POSIX and PowerShell flavors; the hand-maintained 15-name
  list it replaces had already drifted (`poisoned_rejected` is written by
  the node but was not listed, so a bare `+=` there passed silently). The
  derived set is 31 names — `PARITY: ... counters=31 ...` now follows the
  struct, so a new ledger bucket is covered without editing any script.
  Proven by injection: `pulse_stalls += 1` fails (the old list missed it),
  a fresh `pub fuzz_probe: u64` field with a bare `+=` fails with no list
  edit anywhere, both revert green.

- The duplicated bench literals left the docs: the README/GUIDE
  cross-check (`test_bench_numbers_agree_across_readme_and_guide`, 20
  hard-coded tokens) is replaced by
  `test_bench_numbers_match_evidence_artifact` in `tests/doc_sync.rs`,
  which compares both docs against the committed Criterion snapshot
  (`docs/evidence/2026-10-26_bench_snapshot.md`): every `~<number> ns/µs`
  citation must round or truncate to an artifact row (derived
  per-frame/per-lane figures and qualitative `~0` are skipped by rule),
  and a row cited by both docs must be cited with a common value. The
  old test went silent the moment both docs carried the same stale
  number — the new one fails that injection, and fails a one-sided
  wrong value too. Audit found nine stale citations (README `~94/~52/
  ~126` vs measured 93.3/51.2/125.1; GUIDE additionally `~8.0/~21.5/
  ~152/~546` vs 7.9/21.7/156.0/542.2); all docs now quote the artifact.

- The release-tag gate only accepts "marked Unreleased" when the
  `## Unreleased` heading sits above the version's own section with no
  other section heading between the two: a stale marker below the
  section (or inside an old section's body) no longer satisfies the
  gate. The `.ps1` compile-gates battery scans metaphors
  case-sensitively, matching the POSIX gate's `grep`.

- A guard with no watchdog armed takes no admission stamp
  (`needs_stamp`): `armed_at_ms` is written only when a deadline exists,
  and the passthrough dummy never reads the clock. `node.enter + commit`
  drops 124 → ~91 ns, the passthrough canary 96.5 → ~55 ns (50-sample
  Criterion runs, 2026-10-27). The documented cost: `FaultRecord::elapsed_ms`
  and `GuardSnapshot::elapsed_ms` are `0` for guards without a deadline;
  the timing contract is still pinned by a deadline-armed timeout test.
- The token bucket can no longer be refilled twice for the same wall
  time. `now_ms()` is read before the node mutex, so two admissions can
  reach the lock out of order; the old code then wrote the older stamp
  into `rate_last_ms`, and the next admission counted the interval
  again, admitting one extra burst of `rate_per_sec`. The refill now
  lives in one helper that only moves the stamp forward
  (`src/node.rs::refill_tokens`), shared by the sync and async paths and
  pinned by `bucket_stamp_never_moves_backwards` (fails on the old
  formula with `stamp rewound: 900 != 1000`).
- `EnterFuture` honours the documented eviction verdict: when the waker
  queue entry it owns has already been retired by an `EvictOldest`
  evictor, the next poll returns `Ready(Err(EnterError::QueueFull))`
  instead of re-parking into a fresh slot (which burned capacity and
  could evict a bystander). The entry flag is read under the lock that
  stamps it; a settle pop leaves it clear, so that re-park stays honest.
- The ISR shed counter saturates at `u16::MAX` instead of wrapping to
  zero — a shed count of 65,536 no longer reports "0 shed". Implemented
  as a `compare_exchange_weak` loop (no deprecated `fetch_update`, MSRV
  1.70 intact) and pinned by a 65,536-push test.
- Per-lane counters and admission hooks clamp with
  `Priority::MAX_INDEX` and `u32::try_from(..).unwrap_or(u32::MAX)`
  instead of the literal `min(2)` and an `as u32` truncation, so the
  bound follows `Priority::COUNT` and an over-large in-flight reading
  cannot wrap.
- The supervisor no longer clamps an out-of-range `OP_STATE` sequence id
  to chunk 2: a frame whose `task` is not 0/1/2 is malformed and is
  dropped, instead of overwriting the tail chunk and its authoritative
  full-digest CRC.
- `lock_poison_tolerant` documents why skipping `hard_clamp_slots` on the
  recovery path is safe (a foreign mutex poison cannot tear `NodeInner`;
  the holder finishes its own mutation, and `lock()` clamps on its own
  poison path).
- A `core`-only build is warning-clean again: `Priority::MAX_INDEX` was
  `dead_code` without `std`, so the const-assert block now pins
  `Priority::MAX_INDEX + 1 == Priority::COUNT` (the lane table's last
  dense index) and the constant is used in every feature combination —
  no `allow(dead_code)` waiver.
- Sync admission no longer reads the OS clock when the token bucket is
  disarmed. `7481e9a` hoisted `now_ms()` out of the mutex unconditionally
  to keep clock reads off the lock, which made every default admission
  pay one clock read; the bucket's arming is now mirrored outside the
  mutex (`SnubberNode::rate_per_sec`) and the stamp is taken only when a
  token is actually checked. Full 50-sample bench on the same machine:
  `node.enter + commit` 126.0 → 166.2 → 124.2 ns (baseline → regression →
  fix); `enter_erased_auto` 208 → 243 → 205 ns.

### CI and release gates

- The declared MSRV is now built. Every lane used `@stable`, so
  `rust-version = "1.70"` was an unverified claim; a new `msrv` job checks
  the library on Rust 1.70 in three shapes (core, core + alloc, std).
  Proven locally with `cargo +1.70 check --locked --lib` (all three
  green) and a negative probe: a temporary `LazyLock` fails on 1.70 with
  `E0658` (`lazy_cell` unstable). The guide now states the honest scope —
  the dev graph (criterion → clap) needs Rust 1.74+, so tests and benches
  are outside the floor and this lane deliberately does not use
  `-D warnings` (linting stays in the stable quality job; 1.70's
  dead-code lint disagrees on cfg-dependent items).
- `scripts/check_api_diff.ps1` no longer hard-fails when the baseline tag
  is HEAD (the release commit itself): it falls back to the previous tag,
  mirroring the `.sh` gate, and fails only when no previous tag exists.
  `check_compile_gates.ps1` Gate 6 delegates to that script instead of
  duplicating the rule.
- The stress harness refuses to run an external supervisor in CI
  (`CI=true/1` for `.sh` and `.py`; `.ps1` fails on any skip in every
  environment): without a launched child there is no pid for the
  RSS/fd/exit gates, so the run would report without leak evidence.
- `scripts/release.sh` no longer lets a tolerated semver SKIP hide a real
  gate failure. The verdict moved into `scripts/release_gate_verdict.sh`,
  which fails on any `FAIL:`/`FAILED` line or unexpected SKIP before it
  considers the local-only semver SKIP; a non-zero gate exit with no
  tolerated SKIP fails too, and a missing `PARITY:` fingerprint now stops
  the release instead of being grepped with `|| true`. Proven against
  crafted logs: semver-SKIP + `FAIL:` → rc 1, foreign SKIP → rc 1,
  opaque exit 1 → rc 1, tolerated SKIP alone → rc 0 with WARN.
- Gate 2 and Gate 7 of `scripts/check_compile_gates.sh` no longer abort
  on a clean scan. Both did `HITS="$(grep …)"; rc=$?` under
  `set -euo pipefail`: a zero-hit grep killed the script before `rc=$?`,
  so the `OK` branch (and the `PARITY:` line) never printed and the audit
  silently did its best impression of a passing gate. The scans now keep
  their exit code with `|| rc=$?`; proven by running the gates on a tree
  with every masking comment removed — both print `OK` and `PARITY:
  gates=7 counters=15 functions=3 metaphors=5` (before the fix the same
  tree exited 1 with no output at all).
- `termination-smoke` no longer hangs. The test used to launch the
  supervisor itself and then pass `--external-supervisor` (`.sh`) or
  nothing (`.ps1`), which tripped the CI `--launch` gate; the harness
  exited, the supervisor blocked in `child.wait()` waiting for kills that
  never came, and both matrix jobs burned the 20-minute job timeout. The
  test now spawns only the harness with `--launch` / `-Launch` (profile
  debug, matching its own build) and waits on it through a hard deadline,
  asserting its exit status — a refusing or wedged harness fails the lane
  in seconds. The dead `--external-supervisor` flag is removed from
  `stress_harness.sh`; kill-only mode still exists for a locally launched
  supervisor. Measured locally: 25 kills, verdict PASS, harness 0.9 s,
  lane 2.3 s (CI=true: 4.3 s) against the old 20-min cancel.
- The three Miri jobs compile with `RUSTFLAGS="-D warnings"` (on top of
  the existing `-Zmiri-strict-provenance`), so nightly-only lints fail
  the lane instead of scrolling past. `scripts/release.sh` now passes
  `--locked` to `cargo clippy` and `cargo publish --dry-run`, matching CI.
- The core-only lane now denies warnings. Gate 5 in
  `check_compile_gates.{sh,ps1}` and the two `--no-default-features`
  steps in `ci.yml` run under `RUSTFLAGS="-D warnings"`, so a warning on
  bare `core` fails the gate instead of printing and passing. The `.ps1`
  gate saves and restores the caller's `RUSTFLAGS`.

### Documentation

- Public docs no longer name things that do not exist: the bench label
  map and README use the real `success_rate_per_mille`; `rate_per_sec`
  documents its own `RateLimited` column instead of `BulkheadFull`;
  `queue_capacity` is labelled as the alias of the canonical
  `queue_limit`; the async entry module states the real waker-queue
  defaults (`queue_limit` 32, `RejectNew` — not "1024, oldest evicted");
  three dead `docs/FUTURE_TRAPS.md` pointers now cite
  `docs/INVARIANTS.md` (Invariant 3, Verification Protocol); and the
  `no_std` surface names `Protected` as core with `SafeLockExt` as
  `std`. Negative grep (`success_permille`, `FUTURE_TRAPS`,
  `1024, oldest evicted`) is empty; `cargo doc -D warnings` is clean.
- The `no_std` drain quickstart in `README.md` now compiles. It passed a
  `u64` tick value to `PipeDrain::manual_with_clock`, whose argument is a
  `ClockFn` (`fn() -> u64`); README fences are not doctested, so nothing
  caught it. The fixed snippet is duplicated as a doctest on
  `manual_with_clock`, and the badge moved to 428 (401 tests + 27
  doctests, 5 ignored).
- The three pool storm reports are committed under `docs/evidence/`
  (2026-10-26, with a provenance README). README and USER_GUIDE quote the
  committed files — `wall_ms=55/474/194`, 99,628 lease observer samples —
  and say which fields are deterministic counters and which are wall
  clock.
- README explains that `target/bench/snapshot.md` is rendered by
  `scripts/bench.sh` into the gitignored `target/` tree and archived by
  the `bench-full` CI lane; no copy lives in the repository.
- USER_GUIDE states the actual SKIP policies: platform-gated skips print
  and pass locally for `.sh`/`.py` but fail under `CI=true/1`; `.ps1`
  fails on any skip in every environment.
- The layer map is one document again. `src/lib.rs` now describes the
  same five layers as README and the User Guide (Layer 3 is the
  runtime-agnostic async engine, not the `alloc` extensions), states
  that `alloc` upgrades the existing layers in place rather than adding
  one, and the README rows for the supervised entry points, friction
  hooks and `NodeName` carry their real layer (4, 2, 2).
- `missing_docs` is `deny` instead of `warn`: the documented surface is
  already complete, so a new undocumented public item now fails the
  build in every feature combination instead of scrolling past as a
  warning.
- Every layer heading now carries the same canonical name in all three
  public docs (`Core RAII Guard`, `Concurrency Bulkhead`,
  `Runtime-Agnostic Async`, `Process Supervisor`, `Diagnostics &
  Health`); README's Layer 3 was still called "Async Waker Queue".
  `tests/doc_sync.rs` locks the bytes.
- The README benchmark tables are re-measured from two full 50-sample
  runs on 2026-10-26. The provenance paragraph records why the old table
  was not reproducible at its own commit (lock-free rows matched, guarded
  admission rows read 30–40% higher — the clock regression) and the
  contended rows quote full-run artifacts instead of a smoke check.
- The README table now publishes the rows that were measured but never
  listed (Layer 1.5 pulse/ISR, `TaskConduit`, `gateway`, `next_sequence`,
  `stats()`), and `scripts/bench.{sh,ps1}` map all 52 Criterion rows to
  operation labels instead of exporting 34 of them as snake_case names.

### Documentation

- The README latency table drops the `SnubberStats::shed_total` +
  `success_rate_per_mille` micro-bench row (two getter loads, no
  operation to choose); the bench and exporter labels stay. A `doc_sync`
  tripwire fails the suite if a public doc cites `success_permille` —
  a method that has never existed (the real one is
  `success_rate_per_mille`) — or if `src/node.rs` loses either real
  getter.

- The `no_std` clock-injection snippets no longer withhold the signature.
  The USER_GUIDE transport example called
  `PipeDrain::manual_with_clock(app_tick_ms)` with `app_tick_ms` and
  `uart_chunk` undeclared; it now declares
  `fn app_tick_ms() -> u64 { 0 }` and a byte chunk, and the comment names
  the type (`ClockFn = fn() -> u64, NOT a number`) in both docs. A
  `doc_sync` tripwire fails the suite when any public rust fence calls
  `manual_with_clock` without declaring `fn ...() -> u64` in the same
  snippet.

- Bench numbers no longer diverge between docs. The README prose
  (`attach + note_death` ~192/~173 ns, `report()` ~111 ns, lease sweep
  ~517 ns) and 14 USER_GUIDE rows (contended 925/899/146/34 ns, storm
  `wall_ms=486`, transition/priority/auto ~126/~126/~130 ns, gateway
  ~50.5 ns, conflation ~21.6/~16.0 ns, pool rows ~192/~173/~111/~517 ns)
  were stale against the 2026-10-26 Criterion run; every figure now
  quotes that run (`971/923/184/40` contended, `225/187` ambush/escape,
  `107` report, `519` expired_lanes, `474 ms` mixed storm, `90/87/94`
  post-`needs_stamp` entries). A new `doc_sync` tripwire pins 16 shared
  tokens across README and the guide and requires the four dual-site
  numbers (table + prose) twice per doc, so a one-site update fails the
  suite.

- The API & Cost Quick Reference table's `Layer` column is re-audited
  against the guide's own chapter map: 52 rows moved (hang/ISR/pulse
  rows to 1.5, memory/`StateDigest`/orphan/`RestartTracker` rows to 4,
  async admission and `ResumeFuture` to 3, the pool and drain rows to
  4, the §4.8 impedance-bridge rows — conflation, envelope, harness —
  to 4, `SafeLockExt` to 1/`std` (its `protect` module), diagnostics
  rows to 5). `ClosurePolicy` stays 2/`alloc` per the `src/lib.rs`
  layer map. A legend above the table defines the column, and a new
  `doc_sync` tripwire pins 23 representative rows to their layer so the
  column cannot silently rot again.

- The erased-guard bulkhead boundary is stated at preset level: the
  README gained a "Bulkhead Scope (erased guards)" guarantee entry —
  `max_in_flight(N)` bounds concurrent admissions, not live erased
  transactions (`enter_erased_auto`/`spawn_guarded` release the slot at
  admission) — the `enter_erased_auto` doc carries the same note, and a
  `doc_sync` tripwire fails the suite if either disappears.

- `docs/INVARIANTS.md` section 2 gained the "Gate taxonomy": a table
  classifying every compile gate, `doc_sync` check family and the
  test-count badge as either a semantic oracle (compiler, real build,
  real run, committed artifact) or a drift tripwire (text scan standing
  in where stable Rust has no oracle), each with the reason. The gate
  scripts and `tests/doc_sync.rs` headers point at the section. No gate,
  threshold or assertion changed; nothing was deleted — tripwires stay
  because where no oracle exists, a loud drift alarm IS the protection.

- The bench matrices moved out of the selling docs into
  [BENCHMARKS.md](BENCHMARKS.md): the 52-operation table, the contended
  group and the USER_GUIDE quick reference now live in one file, and
  README.md / docs/USER_GUIDE.md keep a link plus the measurement
  conditions. No latency figure remains in README or the User Guide —
  `test_bench_numbers_match_evidence_artifact` now fails the suite if a
  citation reappears there, still checks every BENCHMARKS.md citation
  against the committed snapshot, and compares the two BENCHMARKS.md
  sections against each other per artifact row. The quick-reference
  `Layer` column pin reads the moved table.
- The User Guide no longer narrates release history. The `v0.5` /
  `v0.6.0` header teasers, the version label on the Extensibility
  chapter and the nine `Fixed in …` audit-batch sections are gone; the
  guide now states in its header that it describes the current behavior
  — with no link to this file and no mention of changes at all. The
  audit tables moved verbatim to "Audit-batch fix tables" at the bottom
  of this file, and the EvictOldest one-slot overbooking ceiling is
  stated as current contract in the Layer 2 shed-policy bullet (the
  BENCHMARKS.md link that pointed at the deleted anchor now targets
  Layer 2). A `doc_sync` tripwire fails the suite if the guide carries
  a `Fixed in …` heading, a version-labelled Extensibility heading, a
  `v0.…` header teaser or any mention of `CHANGELOG` again.
- The API & cost quick reference moved back into the User Guide as a
  signature, layer and footprint dictionary with no measurements: the
  measured cost cells were replaced with their structural costs, and
  the no-nanoseconds tripwire keeps it that way. BENCHMARKS.md keeps
  the operation matrix and the contended group (the single numeric
  home); the `doc_sync` layer pins read the guide's table.
- A README audit (readme-forge checklist) fixed four first-screen
  defects: the version stamps `(0.6.0)` / `(0.6.1)` / `(0.6.2)` on the
  Extensibility, Liveness and Audit-Hardened guarantee bullets are gone
  (current state, not history — the same rule the User Guide already
  follows); the `tests-N passed` badge now links to the CI workflow
  instead of an empty `href`; and the demo GIF moved below the fold
  (after Quickstart) off the first screen.
- The termination-storm tables moved out of the README into
  BENCHMARKS.md, and their numbers are re-derived instead of transcribed:
  the old table quoted a 2026-10-22 console run whose raw JSON was never
  committed (and the untracked `target/` copy is gone), so both storms
  were re-run on the same machine (4 lanes × 2,500 = 10,000
  `TerminateProcess`, release profile, 2026-10-27) and the raw
  `multi_lineage_report.json` / `multi_lineage_stats.json` of each mode
  are frozen verbatim under `docs/evidence/` (armed-hold: 23.96 s, 417.4
  kills/s, p50 323.4 µs, 10,000/10,000 ambush; free-running: 14.48 s,
  690.7 kills/s, 1,800 / 67 / 8,133). README keeps only a link;
  BENCHMARKS.md carries the section, including the CI-runner table
  (CI-log figures, marked as such). The guide's storm chapter drops the
  un-derivable 2026-10-22 measured figures (single-lineage 63.7/78.6
  kills/s, 4-lane 390.6/719.8 kills/s, CI rates) and now describes the
  gates themselves, linking BENCHMARKS.md for the numbers. A new
  `doc_sync` oracle derives every headline storm token from the
  committed JSONs at test time and fails on divergence — proven by
  injection (wrong `417.4` → TRIPWIRE → surgical revert → green). The
  storm oracle is the 439th passing test (badge moved 438 → 439, 5
  ignored).
- The README is rewritten from the readme-forge blueprint (write mode)
  rather than patched again: the first screen is now title → one
  guarantee sentence → hygiene line → `scoped_checked` quick start →
  delta → when-not, the ten former guarantee essays are folded into
  eleven bullets under "What it guarantees", and the firmware story sits
  under its own heading after the limits. Measured: prose (excluding
  code, tables and badges) 20,624 → 7,680 characters, whole file 23,112 →
  12,725; prose *lines* read 81 → 104 only because the old file kept
  unwrapped lines up to 1,011 characters. What survived is verifiable
  only: the format ceilings are published constants re-checked in source
  this pass (3 lanes `src/policy.rs:137`, 4 stages `src/node.rs:4562`,
  16 ISR slots `src/isr.rs:30`, 28 B payload `src/drain.rs:17`, 512 B
  snapshot `src/diagnostics.rs:27`, 250 ms EOF grace
  `src/pool/mod.rs:59`, 44 B frame `src/drain.rs:89`, 64 B digest
  `src/supervisor.rs:524`, 16 B envelope `src/envelope.rs:124`), the
  10 000 msg/s → 60 FPS workload is re-derivable with
  `cargo run --example 17_impedance_bridge`, and every measured latency
  left the file (tilde-citation scan: 0 hits; banned-fluff scan: 0 hits).
  All 27 local links resolve, the quick-start signature matches
  `SnubberNode::scoped_checked` (`params, invariant, rollback, work`), the
  `doc_sync` pins (erased-bulkhead scope, layer map, guide anchors) pass
  unchanged at 13/13, and the badge stays 439.
- The first screen now leads with syntax, not with laws: Quick start (demo
  GIF under the snippet) and The delta sit above the eleven guarantee
  bullets. Measured on the rendered file — the first fenced block moved from
  line 69 to line 22, the first `rust` fence from line 77 to line 30, and the
  list lines a reader had to scroll past before any code went 42 → 0. Prose
  weight is unchanged (104 lines / 7,680 characters), and the `doc_sync`
  layer-map and erased-bulkhead pins still pass 13/13: they pin wording and
  format, so reordering a section costs nothing.
- The README sells instead of arguing (owner's copy review, 2026-10-27). Four
  edits: the hook's loose "whoever supervises the work gets a classification of
  what actually happened" is now "the supervising process receives an exact
  post-mortem diagnosis of the failure"; Quick start leads with the two-closure
  `scoped` (`rollback`, `work` — signature verified at `src/node.rs:2490`)
  instead of `scoped_checked` + `snub_check!`, and the invariant-checking form
  is shown one step later in The delta, whose `WITH` block is now one argument
  per line; the six-line erased-guard scope disclaimer is out of the guarantee
  list (it duplicated USER_GUIDE §2.5 "Scope correction"); and the
  "`std` is a real boundary" lecture left "When to use / when not" for the
  `Embedded & no_std` chapter, where the module split and the format ceilings
  (3 lanes, 4 stages, 16 ISR slots, 28 B payload, 512 B snapshot, 250 ms grace
  — all re-checked in source) already live. Prose 104 → 98 lines /
  7,680 → 7,260 characters, guarantee bullets 11 → 10, first `rust` fence at
  line 31, 27/27 local links resolve, banned-fluff and tilde-citation scans 0.
- The badge row is now five instruments in canonical order — GitHub (source
  teleport, because this same file renders on crates.io and docs.rs, where the
  reader is NOT on GitHub), Crates.io, **docs.rs** (was missing entirely;
  `https://docs.rs/snubber/badge.svg` is a live rustdoc-build status, not a
  static pill), CI, license — and the `tests-439 passed` vanity badge is gone.
  The count was a CI-log fact masquerading as a landing-page fact: it duplicated
  the CI badge, needed a hand-edited number or a script run to stay honest, and
  no top-tier crate (anyhow, serde, rayon, tokio, ripgrep) publishes one. All
  five hrefs verified live over HTTPS (crates.io returns 403 to `curl` by bot
  policy, the badge itself is 200). The `#![forbid(unsafe_code)]` / MSRV / `std`
  facts stay in the prose hygiene line rather than becoming a sixth pill.
- The README snippets are now compiled, not eyeballed. `src/lib.rs` gained
  `#[cfg(doctest)] #[doc = include_str!("../README.md")] struct ReadmeDoctests;`,
  so `cargo test --doc` type-checks every runnable Rust block in README.md
  against the real API — the mechanism that would have caught doc bug H5 (a
  snippet with a wrong arity) at build time instead of at reader time. Three
  fences were rewritten to pass it: the Quick start `scoped` block and The
  delta `WITH` block are self-contained `no_run` programs over plain locals and
  one `struct Request { amount: i64 }`, and the `PipeDrain` transport block now
  imports `snubber::PipeDrain` and declares its byte chunk (it previously called
  `feed(uart_chunk)` with no import and no binding). The hand-rolled `WITHOUT`
  half stays illustrative pseudo-code and is the one registered `rust,ignore`
  fence. `cfg(doctest)` keeps the README out of `cargo doc` / docs.rs output;
  docs.rs already builds 0.6.10 at 100% coverage, so the new badge reports a
  live status rather than a new failure. Prose is flat under this change
  (104 → 105 lines, 7,603 → 7,661 characters, measuring non-fence / non-table /
  non-badge lines); the +30 file lines are all inside code fences.
- Registry-side metadata fixed, because the README is only half the landing
  page. `<img src="assets/demo.gif">` rendered on GitHub and nowhere else:
  `cargo package` excludes `assets/` (the GIF is 1.8 MB) and crates.io / docs.rs
  have no repository tree to resolve a relative path against, so the demo went
  to an absolute `raw.githubusercontent.com/Ar3ss12/snubber/main/...` href. The
  manifest's discovery fields were re-tuned against the live crates.io category
  list (`/api/v1/categories`, 58 slugs): `epoch` → `no_std` in keywords (five,
  the cap), and the verified `embedded` category added (four of five) next to
  the crate's bare-metal story. `[package.metadata.docs.rs]` now asks for
  `all-features = true` and `--cfg docsrs`; that cfg is inert until a
  `#[cfg(docsrs)]` gate consumes it, and `feature(doc_cfg)` / `doc_auto_cfg`
  were deliberately NOT added: they are nightly-only, and turning a passing
  docs.rs build red would falsify the badge shipped above.

### Tests

- New `doc_sync::test_readme_badges_are_instruments` pins the row's shape:
  exactly 5 `[![…]](…)` lines, one linking to the repo, the registry, docs.rs,
  the CI workflow and `#license`; no badge may carry an empty `]()`, and no
  badge may publish a test count (`tests-…` / `%20passed`). `doc_sync` 13 → 14.
- `scripts/check_test_count.sh` is retargeted, not deleted: the README has no
  badge to reconcile any more, so it now parses the real `cargo test` log,
  fails on any failure or a parse collapse (the 300-passed floor stays), and
  fails if a numeric test count returns to the README — matching the CI step
  renamed to "Suite Health Gate" and the docs/INVARIANTS.md gate-taxonomy row.
  Negative proof on the injection (a `tests-999%20passed` badge):
  `test_readme_badges_are_instruments` → "TRIPWIRE FIRED: the README badge row
  has 6 badges"; script → "FAIL: README.md publishes a test count again";
  surgical string revert → 14/14 and `OK: suite green and the README publishes
  no test count`.
- The doctest run grew from 27 to 31 fences (30 pass, 1 registered `ignore`):
  the four new ones are the README's Quick start, WITH-delta and `PipeDrain`
  blocks compiling, plus the WITHOUT half standing down.
- Three new `doc_sync` tripwires guard that mechanism rather than its prose,
  because a silently deleted hook costs nothing to notice:
  `test_readme_is_wired_into_doctests` (the `#[cfg(doctest)]` +
  `include_str!("../README.md")` pair must survive in `src/lib.rs`),
  `test_readme_fences_have_a_compile_verdict` (exactly 3 `rust,no_run` fences,
  `rust,ignore` capped at one and still the WITHOUT block, no modifier-free
  fence — a bare fence would execute in CI, and every ignore is unproven code),
  `test_readme_assets_are_absolute` (every embedded `src="…"` is `https://`, and
  a raw.githubusercontent path must exist in the tree at the branch the URL
  names). `doc_sync` 14 → 17.
  Negative proofs, one injection each, surgical string revert after:
  `node.scoped(` → `node.scoped_typo(` in Quick start → doctest E0599
  "no method named `scoped_typo` found for struct `SnubberNode`", `test result:
  FAILED. 29 passed; 1 failed`; absolute GIF href → `assets/demo.gif` and
  `rust,no_run` → `rust` on the transport fence → both gates fire by name with
  their line numbers; `#[cfg(doctest)]` → `#[cfg(anydoc)]` in `src/lib.rs` →
  "TRIPWIRE FIRED: src/lib.rs lost the README doctest hook". After the reverts:
  17/17 doc_sync, 30+1 doctests, full suite green.

- `doc_sync::test_readme_states_erased_bulkhead_scope` became
  `test_erased_bulkhead_scope_stays_documented`: it pinned the README, and the
  owner ordered that paragraph out of the README, so the pin moved with the
  text rather than being dropped — it now requires the same correction in
  `docs/USER_GUIDE.md` ("not a lease on the slot for the worker's lifetime",
  "serialize two live erased workers") plus the unchanged
  `enter_erased_auto` doc window. Negative proof: guide wording changed to
  "…worker's run" → TRIPWIRE FIRED → surgical string revert → 13/13. Suite
  count is untouched (13 → 13, badge 439 passed / 5 ignored).

- New `channel` test closes the senders and asserts the receive side
  settles as `GatewayRecvError::Disconnected`; the snapshot tamper test
  matches the exact `SnapshotError` variants (`BadCrc{want,got}`,
  `BadMagic`, `BadVersion(read)`) instead of a bare `is_err()`.
- Four formerly flaky suites are deterministic now: `checkpoint_budget`
  counts allocations thread-locally (a parallel test thread can no
  longer inflate another measurement's peak); `profiling` replaces the
  1 ms sleep handshake with an AtomicBool handshake;
  `async_gate`'s no-park bound is 500 ms; `affordances` joins the
  paused worker behind a 5 s `recv_timeout` (a deadlock fails in
  seconds, verified by a withheld-resume probe).
- New `isr_bridge` cross-thread FIFO test pushes 10,000 pulse signals
  from one thread through the 16-slot SPSC ring while another pops them,
  asserting opcode/reason/16-bit-sequence order end to end (Miri-scaled
  to 200 pushes; both loops bounded by a 30 s deadline so a lost or
  reordered signal fails instead of hanging). The pressure gauge asserts
  refused pushes, not losses. The suite ships executable coverage for
  the core branches at last: `cargo test --no-default-features --lib`
  and `--features alloc` (19 tests each) run in CI quality, and the
  suite joined the Miri concurrency lane.
- Four gates that could not fail now compare against an independent
  oracle. `conservation_model` drops the
  `admission_total() == started + shed_total()` tautology and checks the
  caller-side ledger instead (folding `evicted` into `shed_total` now
  fails 3 vs 2). `clock_park::no_wdt_feed_is_a_noop` pins a zero-sized
  `NoWdt` and a syscall-free call (a 2 ms sleep in `feed` fails).
  `stress_breaker_churn` replaces `reset_breaker(); assert Closed` with a
  dedicated `trip_after_failures(1)` node that trips, admits one probe and
  closes on commit (a probe that no longer closes reports `HalfOpen`).
  `doc_sync` locks a canonical one-line tagline per layer next to each
  heading in all three docs (deleting one fires the tripwire).
- `tests/state_handoff.rs::out_of_range_state_chunk_is_not_chunk_two`
  (an out-of-range `task` must not promote to chunk 2) and
  `src/isr.rs::shed_saturates_at_the_ceiling_without_wrap`.
- `tests/conservation_model.rs::mixed_policy_columns_conserve_every_attempt`
  drives `EvictOldest` overbooking, the `max + 1` bulkhead ceiling, a
  `Critical`-over-`Background` preemption request, a watchdog timeout,
  commit, planned abort and implicit drop through one node and compares
  every column against an explicit ledger:
  `admission_total() == total_started + shed_total()`, `evicted` and
  `preempted` excluded from the shed side, and `reported[]` verified as
  an alert ledger rather than a conservation term.
- `tests/doc_sync.rs::test_layer_map_is_identical_across_public_docs`
  requires each of the five canonical layer names to appear exactly once,
  byte-for-byte, in `src/lib.rs`, `README.md` and `docs/USER_GUIDE.md`,
  in ascending order; `test_documented_module_paths_exist` fails when
  prose cites a `src/...` module that is not on disk (it caught README's
  `Async Waker Queue` and now guards against stale paths like the old
  `src/async_entry.rs`).

## 0.6.10 (2026-10-26)

Audit-2 closeout (PLAN_AUDIT2 findings F-01..F-26; tag and publish only
on an explicit release order, so this section is documented-ahead of any
tag and the release-identity gate reports WARN until then).

### Fixed

- `SnubberGuard::abort` and `ErasedGuard::abort` now swallow a panicking
  rollback under `std` (`catch_unwind(AssertUnwindSafe(..))`), matching
  the documented contract and the `Drop` path: a planned abort never
  escalates into a double-panic process abort. Without `std` the
  behaviour is unchanged (a rollback panic during unwind aborts the
  process — a Rust rule, documented). `commit` is untouched.
- `PoolLane::note_death` no longer stamps `last_exit`/`last_class` onto a
  successor that attached while the reaper sat in the EOF grace (or after
  that successor's own life and reap advanced the generation). The death
  itself stays counted in the lane counters and returned in `PoolReap`;
  only the "latest" fields belong to the generation that actually died.
- The sync queue horizon is telemetry-only. When live admissions reach
  `queue_capacity`, the node records the `BufferOverflow` gauge
  (`buffer_overflow_total`, console line) but no longer executes
  `policy.action` on the admission that still succeeds. Previously a
  `TripBreaker`/`RollbackAndAlert`/custom policy armed on `BufferOverflow`
  tripped the breaker or latched the UI alert on successful admissions.
  The async `RejectNew` queue-full refusal is a genuine rejection and
  keeps the full policy, including `bulkhead_rejections`.
- Every automatic fault path (guard settle, async TTL timeout, async
  evict-oldest, admission refusals: bulkhead/stale/rate/breaker/poison)
  now bumps `reported[kind]` exactly once via `record_locked`, so
  `metric_samples()` and the console alert see `count >= 1` during the
  incident. The old split (only explicit `report()` and the rate/poison/
  preempt codes wrote the slot) left automatic faults invisible to both
  exporters. `bump_reported` is renamed `bump_column` and books only the
  mechanism column for explicit reports; explicit `report()` and the
  automatic path of the same kind now land on the same column.
- A named `report(FaultKind::BufferOverflow, …)` no longer credits
  `bulkhead_rejections` (a shed/conservation column). A BufferOverflow
  RECORD is capacity telemetry; the sync queue horizon raises it on an
  admission that still SUCCEEDS, so folding it into the shed columns
  broke the conservation law `admission_total() == total_started +
  shed_total()`. The async `RejectNew` queue-full refusal is a genuine
  rejection and still books `bulkhead_rejections`.
- `check_timeout` no longer bumps `reported[Timeout]` by hand;
  `record_locked` owns the slot for every path, so no caller can rewind
  or double a published counter.
- `lock_poison_tolerant` no longer re-locks the node ledger synchronously
  on the recovery path: a caller that held the node lock while recovering
  a poisoned foreign mutex deadlocked on the ledger it already owned.
  Recovery now ticks `poison_recoveries` best-effort via `try_lock` —
  a skipped tick under-reports one recovery; a synchronous re-lock wedged
  the caller forever. The doc comment now says exactly that.

### Documentation

- Every published number now carries a source. The README benchmark
  tables are marked historical (2026-10-24 / 2026-10-25 full 50-sample
  runs, command included); the contended table quotes the archived
  2026-10-25 artifact (925 / 899 / 146 / 34 ns — the previous 918 / 911 /
  140 / 35.5 values match no artifact) and shows the 2026-10-26 smoke
  re-run alongside, with the rule "quote full-run artifacts, not smoke
  checks". Storm numbers now match the 2026-10-25 reports
  (`wall_ms=53/479/199`, 101,290 lease samples) and name their artifacts.
- README no longer names `pool-gates` (an artifact name) as the CI job
  for the pool storms — it is the `release-ignored` job of `ci.yml`; the
  guard bullet no longer claims a `Copy` shell (`SnubberGuard` is not
  `Copy`); `src/lib.rs` says five layers, matching README/USER_GUIDE.
- `docs/INVARIANTS.md`: "four properties" → thirteen invariants (the
  document lists 13); the `mem::forget` example size is corrected to
  1,648 B with the measurement command (`-Zprint-type-sizes`, nightly
  toolchain log) instead of the unsourced 2,288 B.
- Compile asserts added where the docs claim them: `Priority::COUNT == 3`
  and `DEFAULT_EOF_GRACE_MS == 250` are now `const`-checked
  (`src/policy.rs`, `src/pool/mod.rs`).
- USER_GUIDE storm numbers and contended rows synced to the same
  artifacts; `queue_capacity` semantics documented in one place (async
  waker-queue ceiling, sync admission horizon, async `0` → 1 floor).

### Tests

- New `queue_horizon` test pins the documented async asymmetry of
  `queue_capacity(0)`: the waker-queue capacity floors to 1, exactly one
  waiter parks, the second is refused with `QueueFull` and the full
  policy fires (sync `0` means the horizon fires on every admission —
  different semantics, both now tested).
- New `affordances` test proves `lock_poison_tolerant` recovers a
  poisoned foreign mutex while another thread hammers `stats()`: the
  recovery never re-locks the ledger, `poison_recoveries >= 1` and the
  recovered guard reads 0.
- Test honesty (stage 6): `wakes_happen_after_drop_in_node` is now a
  function-scoped order audit (every `notify_all`/`w.wake()` in node.rs
  follows a `drop(inner)` in the same function) instead of a count
  heuristic; the termination example exports `frames_sum`/`arms_sum`/
  `commits_sum` plus a `misclassified_stillborn` contradiction counter,
  and `supervisor_termination` asserts cross-source floors (`arms_sum >=
  ambush + escape`, `commits_sum >= escape`) instead of tautological
  sums; all feed-path benches assert the parse result inside
  `b.iter_black_box` and `bench_hygiene` fails any `.feed(` line whose
  result is not asserted.
- New `tests/rollback_panic.rs`: panicking rollbacks on explicit abort
  return `SnubberEvent::Aborted`, fire the rollback exactly once and
  release the slot (SnubberGuard, node TrackedGuard, ErasedGuard).
  Negative runs verified per root fix (lib.rs, erased.rs): disabling the
  catcher turns the tests red. The erased probe test was updated from
  pinning the old "abort surfaces the panic" behaviour to the
  documented swallow contract. README badge 416 → 419.
- New `supervisor_pool` tests: attach-during-grace (thread + attach
  handshake, no sleep margin) leaves the successor's `last_exit`/
  `last_class` at `None`; a reaper without a successor still stamps its
  own telemetry. Negative run verified: removing the generation/pid
  guard turns the attach test red. README badge 414 → 416.
- New `tests/queue_horizon.rs`: `TripBreaker` and `RollbackAndAlert`
  armed on `BufferOverflow` do not fire on horizon hits over
  `queue_capacity(0)`; the gauge still exports `buffer_overflow_total`
  and the conservation law holds; the async `QueueFull` refusal with the
  same policy still trips the breaker and books `bulkhead_rejections`.
  Negative run verified: restoring `record_locked` in the horizon branch
  turns 2 of 4 tests red.
- New `tests/metrics_reporting.rs`: every automatic path exports exactly
  one metric sample with `count >= 1`, the hook snapshot carries the same
  alert count, named vs automatic reports of one kind agree on the
  reported slot and the mechanism column, and BufferOverflow records
  never enter the conservation law. Negative run verified: removing the
  centralized `reported[]` bump turns 5 of 6 tests red.
- `trap1_no_double_count_implicit_drop_with_reported` now pins the fixed
  contract: one guard settle produces `reported[ImplicitDrop] == 1`
  (previously it pinned the invisible-fault bug as `== 0`).

### Tooling / CI

- The three stress harnesses share one SKIP verdict formula:
  platform-gated skips are printed and allowed locally, but any skip
  fails the run in CI. `stress_harness.sh` counts `SKIPPED` and gates the
  verdict on it unless `CI` is unset; `stress_multi_lineage.py` gained
  the same `CI`-aware rule; `stress_harness.ps1` prints SKIP rows
  (tri-state OK/FAIL/SKIP) and fails the verdict on any skip. Negative
  runs verified against the real verdict lines with synthetic state.
- The Miri leak-lane comment now references the actual exclusions
  (`tests/channel.rs:13` throttle retry, `tests/task_engine.rs:296`
  wall-clock TTL) and documents that the lane intentionally mirrors the
  core suite set — the concurrency suites rely on real-time deadlines and
  process spawns Miri cannot schedule deterministically.
- README no longer claims the heavy kill lanes gate every push: the
  multi-lineage POSIX storm (4×250 armed-hold + 4×500 free-running) and
  a 25-kill termination smoke gate every push/PR; the 200-kill
  single-process harness, the 100-kill PowerShell harness, the Win32
  multi-lineage storm and the full `#[ignore]` termination gate run on
  the weekly schedule or manual dispatch.
- USER_GUIDE documents all three `queue_limit`/`queue_capacity` semantics
  in one place (async waker-queue ceiling, sync admission horizon,
  telemetry-only behavior, and the async `0` → 1 floor).
- `scripts/release.sh` now parses every `SKIP (...)` line in the compile
  gates log and tolerates exactly one, verbatim —
  `SKIP (cargo-semver-checks not installed)`. Any other SKIP fails the
  release; the old condition only checked the presence of the semver
  SKIP, so a combination like "semver SKIP + Gate 5 SKIP" passed.
- Every ignored release lane (`supervisor_pool_stress*`,
  `stress_breaker_churn`, `supervisor_termination`) now requires
  `executed > 0`: a green `0 passed` (renamed test, cfg'd-out module,
  target-filter typo) fails the gate instead of silently passing. The
  same executed>0 check is mirrored in the `release-ignored` and
  `termination-smoke` CI jobs (bash shell on both Linux and Windows).
  Negative proofs: a synthetic log with a foreign SKIP dies; a
  `0 passed` lane log dies; honest logs pass with the executed count.
- README tests badge 404 → 414 (10 new tests from the telemetry and
  queue-horizon fixes); the badge drift gate caught the difference.
- `scripts/release.sh` (plus the new `justfile`) runs the whole release
  gate in one command: fmt, clippy, full suite + doctests + badge drift,
  compile gates, the `#[ignore]` release lanes, the release identity gate
  and `cargo publish --dry-run`. `--fast` stops after the compile gates
  for a pre-commit check. The gap was manual: the 0.6.9 CI failure came
  from a release-only lane that nothing ran before the release.
- The ignored release lanes (pool storms, breaker churn, termination
  smoke) moved from the Stress workflow into `ci.yml`, so they gate every
  push/PR in the main run; the Stress workflow keeps the scheduled storms.
- Miri jobs pin `nightly-2026-10-09` instead of a floating `@nightly`: the
  0.6.9 Miri failure was a std change in a newer nightly, not a repository
  change, so bumping the pin is now a deliberate commit.
- `just check|release-lanes|release|package-dry|publish` wrap the same
  commands in `scripts/release.sh` for humans and agents.

### Tests

- `block_caller_parks_then_admits_freed_slot` no longer bets that a 10 ms
  sleep leaves the freeing commit inside the 50 ms park window. A runner
  can deschedule the committing thread for longer than that margin, after
  which the parked caller legally reports `BulkheadFull` (the CI failure
  was this, not a lost slot: the old shape passed 900 local runs). The
  friction hook now proves the caller reached the full-bulkhead branch and
  blocks until the holder commits, so the re-check after the park always
  sees the freed slot.

### Docs

- `docs/INVARIANTS.md` gains "Test threshold convention: determinism, not
  percentages": volume floors must be tied to the mechanism under test
  (`admitted >= breaker_trips`), never to a share of attempts, and
  best-effort standard-library behavior must not be asserted without
  `cfg(not(miri))`. A companion section, "Test synchronization convention:
  handshake, not sleep margins", forbids sleeping as cross-thread
  synchronization and requires bounded waits.

## 0.6.9 — 2026-10-25

Eighth-audit batch (hook panics, scanner reset order, lock hygiene, honest
tests and benches, CI teeth). No API removals.

### Fixed

- A panicking `friction_fn` / `pressure_fn` / custom backpressure hook no
  longer leaves `half_open_probe` set forever: the probe flag is cleared
  under a fresh lock and the panic is resumed to the caller. A wedged
  breaker used to survive until a manual `reset()`.
- `SnubScanner::push` resets its frame state before invoking `on_token`,
  so a caught panic inside the callback can no longer redeliver the same
  token on the next chunk.
- `publish_fault` swaps `last_fault` and drops the replaced `FaultRecord`
  after the node lock is released; an owned `NodeName` no longer runs
  foreign destructor code inside the critical section. The doc comment
  now also states the one-time `flight` realloc that follows an external
  shrink.
- Re-parking an async waiter replaces its `Waker` with `mem::replace` and
  drops the old waker outside the node lock; `deregister_waiter` removes
  the exact parked entry and drops it outside the lock as well. A waker
  destructor can no longer re-enter or deadlock the node.
- `Conduit` wakes parked futures via `mem::take` (no allocation under the
  wakers lock) and drops a replaced `ResumeFuture` waker after unlock.
- The node reads the clock outside its mutex: rate-limit admission takes
  the timestamp before locking, and `check_timeout` snapshots the settle
  time after rollback but before the fault record is written.
- `PipeDrain::feed` after `finish()` now increments `feeds_after_eof()`
  and reports zero frames for the discarded chunk; the documented clock
  rule matches the code (no clock read under a drain lock).

### Changed

- Documentation names the real receivers and layers: `lane.attach` /
  `lane.note_death` (not `pool.*`), `PipeDrain::{feed, finish, write_to}`
  as `std` transport on top of the `core` scanner, and `pool.report()` as
  one included `Vec<LaneSnapshot>` allocation rather than "no
  allocation". Public accessors for `PoolConfig`, `PoolLane`,
  `PoolReport`, `ExecutionEnvelope`, `ProfileHarness`, `HarnessReport`
  and `ProcessToken::commit` are now listed.
- README bench IDs match the source (`*_threads_6`, `push_pop`,
  `classify_stall_ceiling`), and the latency table carries a measurement
  date, the Criterion command, and the CI artifact lane
  (`bench-criterion-<run_id>`, `scripts/bench.sh --skip-run`).
- Ordering comments now match the code: the gateway depth gauge is
  Relaxed (the authoritative cap is `try_send`; the throttle latch stays
  Release/Acquire), the two free-standing `SeqCst` fences in the erased
  transaction are gone (the ledger mutex is the ordering edge),
  `next_sequence` explains why a Relaxed ticket is enough, and the lane
  `pid`/`drain` pair documents the absence of a panic point between the
  two stores. `task_ttl` is documented as starting at the first park and
  never re-stamping.

### Tests

- New coverage: a winner dropped between settle-pop and re-poll hands the
  free slot to the next waiter; a HalfOpen async probe cancelled in the
  queue does not wedge the breaker; an async admission recovers from a
  deliberately poisoned ledger (`poison_recoveries`, commit,
  conservation). The ISR example now asserts the laser is off after the
  cycle-3 abort as well.
- `stress_breaker_churn` no longer demands that 10% of the storm's attempts
  be admitted. Once the window trips, HalfOpen admits exactly one probe and
  refuses every other attempt, so the lane structurally yields about four
  admissions per trip (release: 301 admitted, 75 trips, 11698 breaker
  refusals). The volume floor is now tied to the churn the lane exists to
  prove (`admitted >= breaker_trips`, `breaker_rejections > 0`) instead of
  to a share of attempts that only held while the storm ran slowly enough
  to admit work before the first trip.
- `fake_waker_identity_matches_allocation` keeps its clone-identity
  assertion only outside Miri. std builds the `Arc` waker and its clone
  vtable as two identical const promotions and relies on LLVM merging
  them, so the clone carries the same vtable pointer natively; Miri never
  runs LLVM and each promotion stays a separate allocation, making
  `will_wake` (best-effort by contract) return false. The engine dedups on
  its own `Arc<AtomicBool>` identity, so no library behavior depended on
  the assertion.

### Tooling / CI

- Both gate flavors print an identical `PARITY:` fingerprint, and a
  Windows CI step fails if the `.sh` and `.ps1` counters diverge; CI
  detection is strict (`CI=true|1` only) in bash and PowerShell.
- New release-identity gate compares `Cargo.toml`, the latest tag and
  `CHANGELOG.md` (warns for a documented-but-untagged version, fails for
  an undocumented one; never creates tags).
- New `miri-leaks` job runs without `-Zmiri-ignore-leaks` over the lib
  and 14 suites; the scheduled `bench-full` job archives Criterion
  output; `termination-smoke` exercises the supervisor kill path on
  Ubuntu and Windows on every push.
- Stress harnesses require `--launch` or an explicit
  `--external-supervisor` under CI; the supervisor test passes the
  latter because it owns the child itself.

## 0.6.8 — 2026-10-23

Seventh-audit batch (core accounting, async liveness, erased slots, honest
examples and gate teeth). Details: USER_GUIDE "Fixed in 0.6.8".

- Conservation is now an API-level invariant:
  `admission_total() == total_started + shed_total()`. `shed_total()`
  counts every first-pass refusal exactly once (capacity, stale, rate,
  breaker, poisoned lock, memory); `evicted` and `preempted` are
  side-events of an admission already in `total_started`, so they no
  longer inflate the shed side. `rate_rejected` is no longer mirrored
  into `bulkhead_rejections`.
- Depth gauge subtracts `explicit_aborts` and `implicit_drops`, so a
  settled transaction never shows as in flight.
- Rate shedding has its own `FaultKind::RateLimited` (index 12,
  `FaultKind::COUNT` 13): a `BulkheadFull` TripBreaker override can no
  longer fire on a paced refusal, hooks and health see the honest event,
  and `rate_rejected` is booked once. `SnapshotV1` bumps to v5 and grows
  the reported tail 12 → 13 (322/508 bytes).
- Admission stamps and fault details are built in PHASE B: every
  rejection arm drops the state lock before `arming.clone()`/`format!`,
  and the rate counter uses `saturating_add` like every other bump.
- Async admission probe state is cleared on every terminal `Ready`
  (spurious re-polls no longer leave `half_open_probe` wedged, and a
  Ready future retires any queue entry it still owns by identity).
  `Drop for EnterFuture` hands the freed slot to the next waiter in
  `deregister_waiter`, so a cancelled task cannot strand a quiescent
  queue; an evictor's verdict clears the flag without waking a phantom.
- `enter_erased` / `enter_supervised_erased` book the success when the
  erased guard's `commit()` runs, never at admission: `committed += 1`,
  one window outcome, streak reset; a guard that never commits cannot
  inflate `success_rate_per_mille`. Rollback books one planned abort
  (`explicit_aborts += 1`, keeping the depth gauge balanced against the
  `total_started` booked at admission) and still re-opens a HalfOpen
  probe.
- Poison recovery: gateway books, conduit drafts and the cancel-reason
  guard recover `into_inner` instead of silently dropping the update
  (`.ok()` issued the notify unlocked, re-opening a lost-wakeup window).
- `Protect` runs the rollback BEFORE taking the resource (same order as
  `Drop`): the closure can still read the protected value.
- `PipeDrain::feed` takes the ingest lock before the EOF check (a
  concurrent close is fully ordered, not half-applied), and the
  injectable clock is read once per chunk OUTSIDE the state lock
  (`apply_token_at`).
- Channel: HWM is computed as `c - c/4` (no overflow at absurd
  capacities) and the throttle depth is loaded with `Acquire` so a
  refusal cannot act on a stale depth.
- Overflow and clamp fixes: `task_ttl` converts `as_millis` with
  `try_from` (saturates, never wraps), restart-window reserve uses
  `saturating_add`, lane histograms are clamped to the aggregate
  `in_flight` after lock recovery, and profile stage slices are clamped
  to `stage_count`.
- Tests, examples, benches: conservation asserted in the invariants,
  bulkhead-ceiling and industrial suites; the pool storms gate app-side
  token receipt and lane lineage (`tokens_seen`, `token_lane_mismatch`),
  so `rescued == ambush` is no longer the only witness; rollback effects
  are observable
  state (DB handles, iron energization, laser latch, real advised sleep)
  instead of counters that can be faked; fake wakers use
  `Waker::from(Arc<dyn Wake>)` (Miri strict provenance); the bit-flip
  fuzzer asserts a non-zero, fully accounted mutant sweep; bench hygiene
  covers `TaskConduit::pair` / `PipeDrain::drain_sync`, rejects
  `std::io::sink()` (a discarding writer understates `write_to`), and
  requires `iter_batched` for per-sample drains so construction stays
  untimed.
- Scripts/CI: a grep error (rc=2), a missing report, a skipped check
  (missing tool or thumb target) and a baseline equal to `HEAD` all FAIL;
  `--locked` on every cargo invocation (thumb gates, publish dry-run);
  `cargo-semver-checks` exposes no `--locked`, so both API gates hash
  `Cargo.lock` around the run and FAIL if it was rewritten; the stress
  harness refuses to publish a verdict when its stats report is missing
  or empty; harnesses fail on zero samples; deadlock literals removed
  from termination reports.
- Settle paths no longer call `mem::forget`: `SnubberGuard` and
  `ErasedGuard` `commit`/`abort` construct their events and let the
  disarmed guard drop normally. The fields were taken first, so the
  forget was harmless, but it skips drop glue for every field — the
  exact shape that leaked one `NodeInner` per transaction from
  `TrackedGuard` before 0.5.2. `#![deny(clippy::mem_forget)]`, a new
  script gate (7/7) and a 250k-guard allocation storm (Layer 1 guards
  owning heap payloads plus the node erased path in commit and abort) keep
  it removed.
- One allocation per erased admission: `ErasedGuard` now holds a single
  boxed `ErasedSettle` transaction instead of separate rollback and commit
  closures, and the node path boxes a transaction that already carries its
  node clone. `enter_erased_auto + commit` measured 301.6 → 207.75 ns
  (two filtered Criterion runs: 209.21 / 207.75 ns mean), with none of the
  settle, abort-bookkeeping or supervisor-token semantics changed.
- Miri runs with `-Zmiri-strict-provenance` in both CI lanes.
- The benchmark exporter publishes only `.../new/estimates.json` (a real
  ns latency): it also published `change/` entries, i.e. a relative delta
  printed as "nanoseconds". CI now runs one bench and greps the exported
  table instead of only compiling benches. The ignored pool storms and
  `stress_breaker_churn` run on every push and PR (`pool-gates`); only
  the supervisor and multi-lineage harnesses stay scheduled.
- Docs: README saturation claim is measured (~1.1M admissions/sec on six
  contending threads), `WorkerPulse` footprint is arch-conditional,
  pool numbers re-measured (attach/reap ~192/~173 ns with the drain built
  outside the timed window, `write_to` ~2.1 ns into a real writer, report
  ~111 ns, lease sweep ~517 ns), the drain and async rows re-measured
  (`PipeDrain::feed` 21.2 → 10.8 us per 256 frames, async ready poll
  ~152 ns, `enter_erased_auto` ~208 ns after the single-box transaction
  replaced two boxes and two `Arc` clones), storm timings re-measured, and the API table
  gains `PoolReap` / `LaneSnapshot` / `PoolTotals` / `expired_lanes` /
  `DEFAULT_EOF_GRACE_MS` / argv-twin rows.
- The POSIX storm harness gates descriptor growth on macOS too: it counts
  `/proc/<pid>/fd` on Linux and parses `lsof -nP -p <pid> -F f` on macOS
  (1 Hz, since `lsof` walks every descriptor), so the four per-lane handle
  checks no longer degrade to platform skips. A skipped check still fails
  the verdict, so the macOS lane is judged on executed checks.
- Two timing-sensitive tests no longer race the clock:
  `spurious_repolls_do_not_reset_the_ttl_clock` ignores Miri and stops
  predicting the poll result from wall-clock time (it accepts either park
  or expiry at the 1 ms-clock boundary and asserts `queue_wait_ms` spans
  from the first park), and `spawn_guarded_binds_thread_guard_leash`
  synchronises through an mpsc handshake instead of a 20 ms sleep.

## 0.6.7 — 2026-10-23

Sixth-audit batch: conduit waker identity, core-exposed drain, honest
bookkeeping, CI/script teeth. Details: USER_GUIDE "Fixed in 0.6.7".

- `ResumeFuture` waker identity is an explicit `Arc<AtomicBool>` flag
  (`Arc::ptr_eq`), not `will_wake`: two futures polled by one task no
  longer cross-match and orphan a sibling's directive-queue entry. Wakers
  are drained from the queue under the lock and woken OUTSIDE it —
  executor code can never run under the conduit's wakers lock.
- The conduit's wakers lock is poison-tolerant (`into_inner` recovery);
  the pause/cancel directive path survives a foreign panic under the
  lock.
- Async queue TTL origin is preserved across spurious re-polls: a
  re-parked waiter keeps its FIRST-park stamp, so a re-poll loop can
  never starve `task_ttl` (regression test:
  `tests/task_engine.rs::spurious_repolls_do_not_reset_the_ttl_clock`).
- `memory_pressure` verdict loads pair with the `Release` store
  (`Acquire`): a pressure refusal can no longer be decided from a stale
  verdict.
- New `drain` module in core: `ProcessToken` / `SnubScanner` /
  `OP_*` / `TOKEN_*` are available without `std` (feed-driven parsing
  on bare metal); `supervisor` re-exports keep every historical path.
- A snapshot const-assert pins the flag region below the CRC region:
  adding `FaultKind`s beyond the wire contract is a compile error, not
  a silent collision.
- Honest refusal buckets in four suites: swallowed `Err(_)` arms are
  gone; every attempt lands in exactly one counted bucket tied to the
  node's own books (`stress_breaker_churn`, `priority_preemption`,
  `transition_orphan`, `industrial`, `bulkhead_capacity`).
- CI: `--locked` on every cargo invocation; a semver-gate SKIP fails
  under CI (`check_api_diff` both flavors); `bench.sh` refuses an empty
  snapshot in every mode and publishes atomically (temp + move).
- Storm harnesses: zero sampling fails the leak gates instead of
  silently skipping them; `stress_multi_lineage.py` reports
  platform-skipped checks as explicit SKIP, not PASS.
- Docs: `failure_rate` documented as a windowed-batch breaker (the
  counters reset after every `window` outcomes — not a continuous
  sliding window); README `no_std`/`alloc` surface list matches the
  module gates; USER_GUIDE cites the TTL regression test and adds the
  `run_supervised_guard_with_grace` / core `drain` API rows.

## 0.6.6 — 2026-10-23

Fifth-audit batch: async liveness, storm-harness hygiene, CI hardening.
Details: USER_GUIDE "Fixed in 0.6.6".

- `ResumeFuture` implements `Drop`: the parked waker is removed from the
  directive queue when the future is dropped mid-`.await` (no leak, no
  stale wake).
- `enter_async_auto` zero-wait re-check no longer refuses a verified-free
  slot: after the post-park admission re-check, a `wait_ms == 0` caller
  (AdviseOnly / Custom pacing) is admitted like any other.
- Async waiter identity is the queue entry itself (own `evicted` flag
  via `Arc::ptr_eq`), not `will_wake` — two `EnterFuture`s inside one
  task no longer cross-match and orphan a sibling's queue entry.
- Async queue TTL now counts from the FIRST park (re-parks refresh the
  waker, not the clock), so queue-wait telemetry and TTL expiry measure
  real waiting time.
- `note_memory_kb` stores the pressure verdict under the bulkhead lock
  (no more out-of-lock window where pressure state could be overwritten
  by a concurrent admission).
- `TaskConduit::update_state` recovers from a poisoned state lock
  instead of silently no-op-ing the update.
- `run_supervised_guard_with_grace`: EOF grace is configurable; the
  pool's `note_death` grace loop backs off with a poll-count bound so a
  non-advancing injected clock cannot spin forever.
- OP_STATE doc corrected to 3 frames; the false-"SNUB"-byte desync
  limitation is documented on `PipeDrain` (wire-format break to fix in a
  major release, not silently).
- Storm harnesses: `stress_multi_lineage.py` kills the lane worker after
  a hard-killed supervisor (no orphaned spinner past `finalized`),
  `stress_harness.sh` traps EXIT/INT/TERM and uses `mktemp` latency
  files; `bench.ps1` refuses to publish an empty snapshot and drops its
  dead `$layer` table; `bench.sh`/`bench.ps1` units unified (`us`).
- Gate scripts: PowerShell flavor is case-sensitive on parity-critical
  gates; the `.sh` Gate 6 no longer swallows the baseline==HEAD warn.
- CI: `permissions: contents: read` on both workflows, `concurrency`
  group + `timeout-minutes` on every CI job, `--locked` on every cargo
  invocation, checkout/upload-artifact re-pinned to current majors
  (v7), and a scheduled/manual Windows lane running the Win32
  `stress_multi_lineage.ps1` storm.
- Tests: `failure_rate` clamp/disable semantics covered (3 new tests),
  `enter_paced_async` both branches (AdviseOnly refusal + admit after
  pace), `print_health` smoke, exact-JSON-number parsing for storm gates
  (`json_num`), token-bucket test de-flaked, storm gate now an
  `#[ignore]` breaker-churn stress; the three numeric storm-gate asserts
  no longer match by substring.
- Docs: erased-guard "iron law" scope correction (the admission probe is
  not a lifetime lease; serialize live workers yourself), wake-window
  semantics in Layer 3, honest `no_std` surface list in the README,
  `PulseTick` reworded as internal, Layer 5 helper exports row.

## 0.6.5 — 2026-10-23

Fourth-audit cleanup batch: every remaining P2/P3 finding closed.

- `OverflowPolicy::EvictOldest` / settle paths / `enter_erased` bulkhead
  semantics: previously implicit behavior is now documented where the
  contract lives (async settle wake window; erased guards are not bounded
  by `max_in_flight` once admitted — enforce an in-flight ceiling in the
  daemon).
- `failure_rate(window, 0)` no longer trips on a fully successful window:
  `percent` is clamped to `1..=100` (`0` reads as "any failure in the
  window trips", not "trip always").
- `PipeDrain::take_armed` / `take_state`: a poisoned lock is recovered
  instead of silently dropping a rescue token (`DrainState` holds no
  tearable invariants).
- `next_sequence` is a pure atomic bump (the bulkhead lock is no longer
  taken to mint a ticket).
- Gate-script hygiene: portable POSIX grep patterns (no `\b` / `\s` /
  `[[:<:]]`), indent-safe `record_locked` extraction, BOM-safe lib.rs
  reads, honest SKIP summary with a skip count, SKIP = FAIL under CI.
- CI: `cargo doc` runs with `RUSTDOCFLAGS=-D warnings`; `Cargo.lock`
  freshness enforced via `--locked`; the PowerShell gate flavor runs on
  the Windows job (with the bare-metal target and `cargo-semver-checks`
  installed there); weekly scheduled re-runs of both workflows; GitHub
  Actions pinned to commit SHAs.
- New tests: `take_stdin_digest` full/torn stdin round-trip, std-side
  contract tests for the firmware-facing `CpuClock` / `IdlePark` /
  `WdtSink` family, a strengthened Condvar-pause regression test.
- Docs: unmeasured cost rows are explicitly marked `structural:`; public
  `CHANGELOG.md` added; API quick-reference rows for `SafeLockExt`,
  `ClosurePolicy`, `TransactionHeader`, `GatewayStats`, `ResumeFuture`,
  `PulseTick`, `LeaseEvent`.

## 0.6.4 — 2026-10-23

Post-release audit strike: one core-concurrency hole and a decorative CI
gate closed with teeth. Details: USER_GUIDE "Fixed in 0.6.4".

- `EvictOldest` overbooking capped at exactly one slot (ceiling `max + 1`);
  beyond it callers are refused with `BulkheadFull`. Before: repeated
  re-booking desynchronized `lane_in_flight` from `in_flight` and
  permanently disabled Critical-over-Background preemption.
- `enter_erased` / `enter_supervised_erased`: the half-open probe settles
  before a panicking rollback resumes — the breaker can no longer weld
  shut.
- Async `EvictOldest` reaches full sync-arm telemetry parity
  (`record_locked` + fault publication + per-kind policy).
- Gate 6 (public API semver) runs for real in CI: full-history checkout
  with tags, `cargo-semver-checks` installed, tag `v0.6.3` as the
  baseline; a SKIP under CI is a failure.
- `stress_harness.sh` kills only identity-verified worker processes
  (image name + monotonic start time).

## 0.6.3 — 2026-10-23

Audit closeout: every finding fixed in one release. Details: USER_GUIDE
"Fixed in 0.6.3".

- `OP_SNAPSHOT` frames are neutral (arm nothing, clear nothing).
- Catastrophic `report()` calls funnel through the same streak trip as
  the guard settle paths.
- Ghost async waiter eliminated: a future admitted on a spurious re-poll
  deregisters its queue entry unconditionally.
- `take_state` consumes the digest; `last_state` is the non-consuming
  peek.
- Generation-0 children get `Stdio::null()` stdin.
- `admission_total` parity between `SnapshotV1` and `SnubberStats` (both
  exclude `evicted`).
- Harness/gate hardening: launch-without-report = FAIL, Gate 3
  empty-extraction = FAIL, line-anchored attribute greps, wedged
  supervisors are reaped and failed.

## 0.6.2 — 2026-10-23

Audit hardening: 13 behavioral fixes. Details: USER_GUIDE "Fixed in 0.6.2".

- Default `BulkheadFull` action is `DiscardSilently` (capacity shedding
  never trips the breaker); an explicit per-kind `BreakerOpen` still sheds
  immediately.
- The HalfOpen probe settles on the erased guard's real outcome.
- `IsrBridge::try_pop` drains malformed cells instead of delivering them.
- One probe at a time in HalfOpen.
- Watchdog timeouts feed the streak breaker.
- A `BulkheadFull` rejection no longer burns the caller's epoch.
- Custom friction advisories are clamped to the node's pacing ceiling.
- Supervisor EOF wait is a bounded 250 ms grace, not a busy spin.

## 0.6.1 — 2026-10-23

Concurrency liveness: six fixes, each with a deterministic regression
test. Details: USER_GUIDE "Fixed in 0.6.1".

- Gateway depth drift fixed (signed counter; the throttle latch can no
  longer wedge on an empty queue).
- A timed-out HalfOpen probe re-opens the breaker instead of wedging it.
- `EpochGate` adopts a new generation at epoch 0 (reboot re-handshake).
- `EvictOldest` wakes the evicted async waiter (its poll resolves
  `QueueFull`, never a zombie park).
- A future dropped while parked deregisters its own queue entry.
- Lane generation bumps before the EOF grace; conduit wakeups are issued
  under the waiter's mutex.

## 0.6.0 — 2026-10-23

Worker pool + true `no_std` + API extensibility.

- `pool`: N isolated worker lineages per process — lanes with attach /
  `note_death` rescue semantics, lease sweep, eviction, per-class
  counters, multi-lineage storm harness.
- True `no_std` (`core` layer builds for `thumbv7em-none-eabihf`),
  `PipeDrain::feed` / `ProcessToken::write_to` drain-feed transport,
  injectable clocks (`MonotonicMs`, `ClockFn`).
- Extensibility: `#[non_exhaustive]` public enums, `Arc` friction /
  pressure handlers, `snub_check_map!`, dynamic `NodeName`, supervisor
  argv API, format-ceiling contract.

## 0.5.3 — 2026-10-22

Benchmark coverage + re-measurement of the whole public surface; POSIX
storm harness and CI wiring.

## 0.5.2 — 2026-10-22

Settlement `Arc` leak eliminated with zero-leak gates; multi-lineage
storm with honest kill classification.

## 0.5.1 — 2026-10-19 / 2026-10-20

Three-circuit hang classifier (`CpuSpin` / `SleepBlocked` /
`DeadlockStall`) with the `DeadlockSuspected` settle verdict;
builder-sealed pass-through canary with a differential proof.

## 0.5.0 — 2026-10-18

Priority preemption, supervisor restart policies, invariant hardening.

---

## Audit-batch fix tables

The per-audit contract tables (root cause → contract now → regression
test) that used to live at the bottom of the User Guide, moved here
verbatim: the guide describes the current state, this file owns the
history. Each table matches the version section above it by name.

## Fixed in 0.6.1: concurrency liveness

Six audit-driven fixes; each has a deterministic regression test.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| Gateway depth drift | A take racing the producer's increment skipped its decrement; the counter crept up until the throttle latch wedged on an empty queue | Signed counter: every send adds exactly 1, every take subtracts exactly 1; `depth()` saturates at 0 | `tests/channel.rs::depth_counter_returns_to_zero_after_concurrent_storm` |
| HalfOpen probe wedge | `check_timeout` on a probe left `half_open_probe` armed forever — the breaker refused everything | A timed-out probe re-opens the breaker (same contract as an implicit-dropped probe); `reset_breaker()` restores traffic | `tests/circuit_breaker.rs::probe_timeout_reopens_breaker_instead_of_wedging_half_open` |
| EpochGate gen-0 adoption | A new generation was adopted and its epoch-0 packet rejected — the rebooted client's first packet (and every retransmission) was lost | A new generation is always a fresh re-handshake, epoch 0 included; "epoch 0 is never fresh" applies within one generation only | `src/lib.rs` tests: `generation_bump_at_epoch_zero_is_a_fresh_handshake`, `node_gate_adopts_new_generation_at_epoch_zero` |
| `EvictOldest` waker loss | The oldest parked async waker was dropped without `wake()` — the task hung as a zombie | The victim is marked and woken; its next poll resolves to `Err(EnterError::QueueFull)` | `tests/async_primitives.rs::evict_oldest_wakes_the_evicted_future_with_queue_full` |
| Dropped parked future | A future dropped while parked left its waker in the queue forever, starving later admissions | `Drop` deregisters exactly its own queue entry (identity flag, no allocation on the ready path) | `tests/async_primitives.rs::dropped_parked_future_deregisters_its_queue_slot` |
| Lane generation timing | `note_death` bumped the lane clock AFTER the up-to-250 ms EOF grace — a successor attaching inside the window inherited the dead child's generation | The clock bumps before the grace wait: `attach()` during the window always returns a fresh generation | `tests/supervisor_pool.rs::successor_gets_fresh_generation_during_eof_grace` |
| Conduit lost wakeup | `resume()` / `send_directive` called `notify_all` without holding the mutex the sync waiter holds across its check-to-park window | Notifications are issued under the same `cancel_reason` mutex — check-then-park and store-then-notify are mutually exclusive | `tests/conduit.rs::resume_never_loses_the_wakeup_under_pause_churn` |

Bench group `layer2_async_waiters` (ready poll and park/wake/admit
roundtrip; numbers in [BENCHMARKS.md](../BENCHMARKS.md)) pins the async
admission path.

## Fixed in 0.6.2: audit hardening

Second audit pass; behavioral fixes, each with a regression test.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| Bulkhead shedding vs breaker | Default `FaultAction` for `BulkheadFull` was `BreakerOpen`: capacity saturation closed a HalfOpen probe as if work had succeeded | Default is `DiscardSilently` (counted, never trips — Capacity Shedding law); an explicit per-kind `FaultAction::BreakerOpen` still sheds the caller immediately, overriding `AdaptiveBlock` parking | `tests/backpressure.rs::explicit_breaker_open_action_sheds_without_parking` |
| Erased probe settlement | `enter_erased` / `enter_supervised_erased` committed a throwaway probe at admission — the breaker went Closed before the real work ran | The probe settles on the erased guard: `commit()` = Closed, drop/abort = Open; nothing else closes it meanwhile | `tests/erased.rs::erased_admission_does_not_close_half_open_breaker` |
| `IsrBridge::try_pop` vs malformed cells | An opcode-0 cell consumed a ring slot and returned a zeroed `IsrSignal` to the caller | `try_pop` drains malformed cells internally and keeps walking; a valid signal behind them is always delivered | `tests/isr_bridge.rs::malformed_cell_does_not_stall_the_drain` |
| Second probe in HalfOpen | An implicit drop while already HalfOpen armed a second probe; the first stayed half-settled | One probe at a time: the streak path only re-arms from Closed/Open, never HalfOpen | `tests/circuit_breaker.rs::implicit_drop_in_half_open_does_not_arm_a_second_probe` |
| Watchdog feeds the streak breaker | `check_timeout` TimedOut settled counters but never counted toward `trip_after_failures` | Non-probe timeouts feed the streak like any other failure and can trip the breaker | `tests/circuit_breaker.rs::watchdog_timeouts_trip_the_streak_breaker` |
| Bulkhead rejection burned the epoch | The sync admission path adopted the epoch before the bulkhead decision — a `BulkheadFull` rejection made the caller's retry a `StaleEpoch` | The epoch is adopted only at PHASE C arming; admission checks freshness with `peek` (no adoption), so a shed caller can retry the same packet | `tests/bulkhead_capacity.rs::bulkhead_rejection_leaves_the_epoch_free_for_retry` |
| Custom advisory uncapped | A custom `friction_fn` could return `retry_after_ms` far above the node's own pacing ceiling | The `Custom` verdict's advisory is clamped to the node's configured cap | `tests/backpressure.rs::custom_pressure_advisory_is_capped_by_the_node_ceiling` |
| Supervisor EOF busy spin | The drain wait spun `yield_now` 50 times regardless of the child's exit progress | Bounded 250 ms grace with 1 ms sleeps (`DEFAULT_EOF_GRACE_MS`), then proceeds | existing `supervisor_memory_loop` coverage |
| Soft verdict honesty | `[SUPERVISOR:MEMORY_DRAIN]` read like a command to the child, but the pipe is one-way and nothing was sent | Log line ends `action=no_kill_self_drain_expected`; docs state the worker self-drains via its own `SnubberNode` soft limit | `tests/supervisor_memory.rs` (log-tag asserts unchanged) |
| Lineage bump tag | `[SUPERVISOR:RESTART]` printed although the one-shot call never spawns a successor | `[SUPERVISOR:LINEAGE_BUMP] gen=N restart=caller_decides` | no test asserted the old tag |
| Harness burst counters | `ProfileHarness` burst/streak counters wrapped on `usize` overflow | Saturating arithmetic | `tests/harness.rs` (unchanged determinism) |

Documentation-only corrections in the same pass: `eof_grace_ms = 0` (poll
once, instant `Deadlocked`) and `u64::MAX` (unbounded) are documented on `PoolConfig` and
`PoolLane::note_death`; `reap_timeouts` is a documented no-op touch-point (watchdog
settlement is cooperative via `check_timeout`); `INVARIANTS.md` tags the
Windows-only stress command with its POSIX equivalents.

## Fixed in 0.6.3: audit closeout

Third audit pass; every finding fixed in one release.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| `OP_SNAPSHOT` frame | `apply_token` treated the cold-diagnostics opcode as an arm: a lone snapshot frame incremented `arms`, and a snapshot arriving between an arm and its commit looked like a second armed transaction | The opcode is neutral: it arms nothing and clears nothing; the next `OP_COMMIT` or emergency opcode still settles the armed token | `tests/drain_feed.rs::snapshot_marker_neither_arms_nor_clears` |
| `report()` feeds the streak breaker | Catastrophic `report()` calls incremented `consecutive_failures` but never tripped — an Open breaker stayed welded until `reset` instead of re-arming its probe | `report()` funnels through the same streak trip as the guard settle paths: Closed trips to HalfOpen (trip counter +1), Open re-arms the probe | `tests/circuit_breaker.rs::reported_failures_re_arm_the_probe_from_open` |
| Ghost async waiter | An `EnterFuture` admitted on a spec-legal spurious re-poll left its queue entry parked forever — the bulkhead permanently lost one slot | The future deregisters its waiter on `Drop` unconditionally (identity-matched, idempotent); a re-park retires the superseded entry first | `tests/async_primitives.rs::admitted_on_spurious_repoll_leaves_no_ghost_waiter` |
| `take_state` honesty | `take_state` peeked and returned the digest without clearing it — "take" lied, and a stale digest could be re-handed to a second replacement | The call consumes the digest: the next call returns `None`; `last_state` remains the non-consuming peek | existing supervisor tests (peek/consume split) |
| Gen-0 child stdin | The generation-0 child inherited the supervisor's TTY on stdin while the pipe is one-way child→out | Stdin is `Stdio::null()`: a worker following the `take_stdin_digest` doc reads EOF immediately | behavioral, gen-0 spawn path |
| Lane `last_exit` after attach | A lane attached to a replacement worker kept the previous worker's exit code until the new one died | `attach` clears `last_exit`: death classification reflects only the current worker | existing `supervisor_pool` coverage |
| EvictOldest telemetry | The queue-horizon overflow recorded a bulkhead rejection although the call was NOT refused | Recorded as telemetry only (bump 0): counters match what callers actually observe | Gate 3 (record_locked body) covers the table |
| `admission_total` parity | `SnapshotV1::admission_total` added `evicted` while `SnubberStats::admission_total` did not — two public counters for the same name disagreed | Both exclude `evicted`; the +1 overbooking of an EvictOldest admission is already counted in `total_started` | `src/diagnostics.rs` (in-file parity asserts) |
| Pending-PASS harness loophole | A supervisor harness that launched a child but died before writing the report still printed PASS | Launch mode requires the report; missing report = FAIL; the deadlock check requires the `deadlock` key in the JSON (a malformed report can no longer false-pass) | `scripts/stress_harness.ps1`, `scripts/stress_harness.sh`, `scripts/stress_multi_lineage.ps1` |
| Gate 3 empty-body false-pass | The zero-panic-under-lock gate silently passed when `fn record_locked` was renamed or moved (extracted body was empty) | Both the `.sh` and `.ps1` gates FAIL on an empty extraction | `scripts/check_compile_gates.sh`, `scripts/check_compile_gates.ps1` |
| Attribute-anchor greps | Gate greps matched the veto/no_std attributes anywhere in a line (a commented-out copy would satisfy them) | Anchored to line start (`^#!\[`) in both gate flavors | `scripts/check_compile_gates.sh`, `scripts/check_compile_gates.ps1` |
| Wedged supervisor harness | The `.sh` harness waited 600×0.1 s for a child that wedged before EOF and then reported the stale state | After the grace window the harness kills the child (`kill -9`) so the verdict reflects the wedged run, not a hung wait | `scripts/stress_harness.sh` |

Documentation-only corrections in the same pass: `AsyncGate` construction
shown as `AsyncGate::new(node)?`; grace/no-op semantics documented on
`PoolConfig` and `PoolLane::note_death`; `WorkerPulse` memory comment
distinguishes 4 B without a 64-bit CAS; EvictOldest sample notes the
transient `max + 1` admission; `note_death(Some(code), ..)` examples match
the API; INVARIANTS.md states the EvictOldest `C + 1` exception to
Invariant 2; The bench matrix measures `commit` allocation-free.

## Fixed in 0.6.4: post-release audit strike

Fourth pass found one core-concurrency hole and a decorative CI gate; both closed with teeth.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| `EvictOldest` ceiling | Every saturated call re-booked: with `max + 2` live guards `hard_clamp_slots` erased the surplus from `in_flight` while `lane_in_flight` kept counting — the Critical-over-Background preemption precondition (`lane_in_flight[0] == in_flight`) could never hold again, silently disabling preemption | Overbooking is capped at exactly one slot (ceiling `max + 1`); a caller arriving beyond it is refused with `BulkheadFull` like any other shed | `tests/bulkhead_capacity.rs::evict_oldest_overbooking_caps_at_max_plus_one` |
| Panicking erased rollback | In `enter_erased` / `enter_supervised_erased` the probe settle lived in the same hook AFTER the user rollback: a panicking rollback unwound past it, `ErasedGuard::drop`'s `catch_unwind` swallowed the panic, and the breaker stayed welded (stale `half_open_probe`) until a manual reset | The settle runs BEFORE the panic resumes (`catch_unwind` around the rollback, settle, `resume_unwind`): the books are always honest, the panic still reaches the caller of `abort()` / `commit()` | `tests/erased.rs::panicking_rollback_still_settles_the_erased_probe` |
| Async eviction telemetry | The async `EvictOldest` arm bumped only the raw `stats.evicted` counter: no `record_locked`, no `FaultRecord`, no per-kind policy — a user's `.policy(EvictedOldest, ..)` never fired on the async path | Full sync-arm parity: `record_locked` (bump 4) + `publish_fault` + `emit`, so the per-kind policy and hooks fire and the counter is counted exactly once | `tests/async_primitives.rs::async_eviction_reports_evicted_oldest_telemetry` |
| Gate 6 in CI (decorative) | `cargo-semver-checks` never ran: CI checkout had no tags (`SKIP`), the tool was never installed, and both SKIPs still printed "All invariant gates PASSED" with exit 0 | `ci.yml` fetches full history + tags, installs `cargo-semver-checks`; under `$CI` a SKIP is a FAIL (`.sh` gate), and the repo is tagged per release (`v0.6.3` baseline) | `scripts/check_compile_gates.sh` CI mode |
| Harness kill identity (.sh) | `stress_harness.sh` followed the pid file / `pgrep` fallback straight to `kill -9`: a recycled pid, or any process whose argv merely CONTAINS the target path (a `tail -f` on the log, an editor), could be killed | Identity gate before every kill: `/proc/<pid>/exe` image name + monotonic starttime vs the last victim (Linux), image-name match on macOS/BSD — parity with the `.ps1` / `.py` killers | `scripts/stress_harness.sh::target_is_worker` |

Documentation-only: `README.md` now attributes the correct verdict names
to `classify_stall` (`CpuSpin` / `SleepBlocked` / `DeadlockStall` — the
settle verdicts `SpinDetected` / `DeadlockSuspected` belong to
`guard.check_timeout()`); INVARIANTS.md states the enforced $C+1$
refusal.

## Fixed in 0.6.6: fifth-audit batch

Async liveness, storm-harness hygiene, and CI hardening.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| `ResumeFuture` waker leak | A future dropped mid-`.await` (e.g. by `tokio::select!`) left its waker in the conduit directive queue: the queue grew per cancellation and stale wakers were woken on the next directive | `Drop` removes the future's own parked waker (identity `will_wake` match under the wakers lock); no leak, no stale wake | `tests/conduit.rs::dropped_resume_future_removes_its_parked_waker` |
| Zero-wait refusal after re-check | `enter` with `wait_ms == 0` (AdaptiveBlock `max_wait_ms: 0` / zero-friction custom curve) refused with `BulkheadFull` even when the slot freed while the caller was inside PHASE B (friction runs OUTSIDE the lock) | `else if in_flight >= max_in_flight` — the verified-free slot is admitted; a genuinely full bulkhead still refuses | `tests/bulkhead_capacity.rs::zero_wait_caller_admits_a_slot_freed_during_friction` |
| Async waiter identity | Re-poll dedup used `will_wake`, so two `EnterFuture`s inside one task cross-matched and the sibling's queue entry was orphaned | Identity is the queue entry's own `evicted` flag (`Arc::ptr_eq`); re-park replaces the waker on that entry only | `tests/async_primitives.rs::enter_async_repeated_polls_do_not_bloat_waiter_queue` |
| Async queue TTL origin | `enqueued_at_ms` never reset on re-parks: TTL/queue-wait measured from construction of the future, not from actual waiting | Stamped at the first park; TTL measures real queue time — and spurious re-polls never re-stamp it | `tests/task_engine.rs::spurious_repolls_do_not_reset_the_ttl_clock`, `tests/async_primitives.rs::dropped_parked_future_deregisters_its_queue_slot` |
| Conduit poison | `update_state` silently returned on a poisoned state lock — the pause/cancel directive update vanished | Lock is recovered (`into_inner`) and the update applied; poison bookkeeping stays with the poisoner | `tests/conduit.rs` |
| EOF grace forever-loop | `note_death`'s grace loop compared only against the clock: a non-advancing injected clock (firmware, tests) spun forever without a verdict | Poll-count backstop `polls >= grace*2` breaks without a deadlock verdict (a verdict needs a clock) | `tests/supervisor_pool.rs` grace window |
| Storm-gate JSON parsing | Three storm gates asserted numbers by substring (`"reaped":32` matches `"reaped":320`): a collision false-passed the gate | Shared `json_num` parses the exact field value; mismatch is a loud FAIL | `tests/common/mod.rs`, three storm tests |
| Harness worker orphan (.py) | After a supervisor hard-kill the worker kept spinning past `finalized=True` — the supervisor that would reap it was dead | The lane worker is killed right after the hard kill, identity-gated | `scripts/stress_multi_lineage.py` |
| PS gate case sensitivity | The PowerShell gate flavor matched case-insensitively where the `.sh` gate is case-sensitive (unsafe scan, counter gate, lock-hygiene, attribute anchors) | `-CaseSensitive` / `-cmatch` on the parity gates | `scripts/check_compile_gates.ps1` |

Scope corrections (documentation-only): the erased "iron law" section now
states that the admission probe is not a lifetime lease on the slot —
`max_in_flight(1)` does not serialize two live erased workers; Layer 3
documents the settle wake-window semantics (a wake is a hint, not a
reservation); the README lists the real `no_std` surface; `PulseTick` is
documented as internal; the Layer 5 helper exports got a quick-reference
row.

## Fixed in 0.6.10: second-audit (PLAN_AUDIT2) batch

Telemetry honesty, horizon semantics, abort contract, gate teeth, provenance.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| Automatic fault export | Guard settles, async TTL timeouts, evictions and admission refusals left `reported[kind]` at 0, so `metric_samples()` and the console alert showed `count=0` during an incident | Every automatic path bumps `reported[kind]` exactly once via `record_locked`; explicit `report()` and the automatic path of one kind land on the same slot | `tests/metrics_reporting.rs` |
| BufferOverflow conservation | A named `BufferOverflow` record credited `bulkhead_rejections`, widening `admission_total() == total_started + shed_total()` | BufferOverflow is capacity telemetry, never a shed column; the async `QueueFull` refusal still books `bulkhead_rejections` | `tests/metrics_reporting.rs` |
| Queue horizon | The sync horizon executed `policy.action` on an admission that still SUCCEEDED, tripping breakers and latching alerts on healthy traffic | Telemetry-only: `record_telemetry_locked` books the gauge (`buffer_overflow_total`, console line) and skips every policy command; the async `RejectNew` refusal keeps the full policy | `tests/queue_horizon.rs` |
| `queue_capacity(0)` | One field, three undocumented semantics; async `0` silently behaved as `1` | Sync `0` = horizon fires on every admission; async `0` = waker queue floored to 1 (one waiter, then `QueueFull`) — documented in one place and pinned by tests | `tests/queue_horizon.rs` |
| Lane death telemetry | `note_death` stamped `last_exit`/`last_class` onto a successor that attached during the EOF grace (or after its own reap) | The stamp is guarded by `generation && pid.is_none()`; the death stays in lane counters and `PoolReap` | `tests/supervisor_pool.rs` |
| `abort()` rollback panic | An explicit `abort()` ran the rollback bare, so a panicking rollback escalated into a double-panic process abort — against the documented contract | Under `std` the rollback runs in `catch_unwind(AssertUnwindSafe(..))`, same as `Drop`; without `std` a rollback panic during unwind aborts (a Rust rule, documented) | `tests/rollback_panic.rs` |
| Poison recovery relock | `lock_poison_tolerant` re-locked the node ledger synchronously on the recovery path — a caller holding the node lock deadlocked | Recovery ticks `poison_recoveries` best-effort via `try_lock`; the doc says a skipped tick under-reports, a re-lock wedges | `tests/affordances.rs` |
| Test honesty | `wakes_happen_after_drop` counted instead of ordering; the termination "conservation" was tautological; feed benches never checked the parsed frame count | Function-scoped order audit; cross-source floors plus a `misclassified_stillborn` contradiction counter; every `.feed(` bench asserts its result (banned by `bench_hygiene` otherwise) | `tests/lock_hierarchy.rs`, `tests/supervisor_termination.rs`, `tests/bench_hygiene.rs` |
| Harness SKIP verdicts | Three harnesses, three SKIP policies: sh printed skips and passed, py failed on any skip, ps1 never printed them | One formula where CI is probed (sh, py): platform-gated skips print and pass locally, any skip fails in CI (`CI=true/1`); ps1 remains stricter — it fails on any skip in every environment | `scripts/stress_harness.{sh,ps1}`, `scripts/stress_multi_lineage.py` |
| Docs provenance | Unsourced numbers: contended latencies matching no artifact, storm `wall_ms` diverging from reports, "four properties" over 13 invariants, 2,288 B `NodeInner`, `pool-gates` job name, a `Copy` shell `SnubberGuard` does not have, "Three layers" in lib.rs | Every published number carries a date + command; contended table = archived full run with the smoke re-run shown; storm numbers name their artifacts; 1,648 B with the measurement command; `Priority::COUNT` and `DEFAULT_EOF_GRACE_MS` are now actually const-asserted | `README.md`, `docs/INVARIANTS.md`, `src/policy.rs`, `src/pool/mod.rs` |

## Fixed after 0.6.10: third-audit (PLAN_AUDIT3) batch

Async eviction verdict, ISR counter saturation, bounds that follow
`Priority::COUNT`, malformed handoff chunks, committed storm evidence.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| Evicted waiter re-poll | A future whose queue entry an `EvictOldest` evictor had already retired re-parked into a fresh slot on its next poll, burning capacity and evicting a bystander | The entry flag is read under the lock that stamps it; a retired future gets `Ready(Err(EnterError::QueueFull))` on its next poll (the documented verdict). A settle pop leaves the flag clear, so that re-park stays honest | `tests/async_primitives.rs::evict_oldest_wakes_the_evicted_future_with_queue_full` |
| ISR shed counter | `shed_count()` was a `u16` incremented with a plain add: 65,536 sheds wrapped to 0 | Saturating CAS stops at `u16::MAX`; a full ring still sheds, the trip latch is untouched | `src/isr.rs::shed_saturates_at_the_ceiling_without_wrap` |
| Per-lane bounds and hook casts | Per-lane indexing clamped with the literal `min(2)`; admission hooks cast in-flight counts with `as u32` | `Priority::MAX_INDEX` follows `Priority::COUNT`; hooks use `u32::try_from(..).unwrap_or(u32::MAX)` | full suite (inputs in `0..Priority::COUNT` behave identically) |
| Out-of-range `OP_STATE` chunk | A frame whose `task` was not 0/1/2 was clamped to chunk 2, overwriting the tail chunk and its authoritative full-digest CRC | Out-of-range ids are malformed and dropped; the chunk index must be `< 3` | `tests/state_handoff.rs::out_of_range_state_chunk_is_not_chunk_two` |
| Poison-recovery clamp | The `hard_clamp_slots` skip in `lock_poison_tolerant` was undocumented | The comment states why it is safe: a foreign mutex poison cannot tear `NodeInner`, the holder finishes its own mutation, and `lock()` clamps on its own poison path | `src/node.rs` |
| Storm evidence | Storm numbers cited reports that live only in the gitignored `target/` tree and are overwritten by every re-run | The three 2026-10-26 reports are committed under `docs/evidence/` with a provenance README; README and this guide quote that content | `docs/evidence/README.md` |

Release gates follow the same batch: the three Miri jobs compile with
`RUSTFLAGS="-D warnings"` on the pinned nightly; `scripts/release.sh`
passes `--locked` to clippy and to the publish dry-run; the stress
harness refuses any kill-only run under CI (`--launch` is mandatory:
without the supervisor pid there is no RSS/fd/exit evidence); and the
Windows API-diff gate, like the shell one,
diffs a release commit against the previous tag instead of failing.

## Fixed in 0.6.8: seventh-audit batch

Accounting conservation, async liveness, erased booking, honest examples,
gate teeth.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| Admission conservation | `admission_total` summed `total_started`, the refusal columns AND `evicted`; a sync `EvictOldest` overbook made the identity impossible to hold | `shed_total()` counts every first-pass refusal exactly once; `evicted` / `preempted` are side-events of an admission already in `total_started`; `admission_total() == total_started + shed_total()` | `tests/invariants.rs`, `tests/bulkhead_capacity.rs`, `tests/industrial.rs` |
| Rate mirror | A `RateLimited` refusal incremented both `rate_rejected` and `bulkhead_rejections` (one shed counted twice) | `rate_rejected` only | `tests/invariants.rs` |
| Depth gauge | `in_flight` proxy was `total_started - committed`, so aborted and implicitly dropped transactions looked in flight forever | Subtracts `explicit_aborts` and `implicit_drops` too | `tests/node_stats.rs` |
| Async terminal `Ready` | An early refusal left the queue entry parked (ghost squatting `queue_limit`); a spurious re-poll could leave `half_open_probe` set | Every terminal `Ready` retires the entry this future still owns (identity-matched) and clears probe state | `tests/async_primitives.rs::admitted_on_spurious_repoll_leaves_no_ghost_waiter` |
| Cancellation handoff | A task dropped between settle-pop and wake left the slot idle until the NEXT settle | `Drop for EnterFuture` observes the free slot in `deregister_waiter` and wakes the next waiter | `tests/async_primitives.rs` |
| Erased admission booking | A throwaway `commit()` at admission wrote `committed += 1` and a clean breaker outcome before any work ran | Admission settles the slot only; the erased guard's real `commit()` books `committed`, one window outcome and the streak reset | `tests/supervisor.rs`, `tests/erased.rs` |
| Erased rollback booking | A rollback (drop/panic) booked NO settle counter: the `total_started` from admission stayed unmatched and the depth gauge reported a ghost in-flight transaction forever | Rollback books one planned abort (`explicit_aborts += 1`) and re-opens a HalfOpen probe; the failure streak is untouched (sync `abort()` parity) | `tests/erased.rs::erased_rollback_books_an_abort_and_clears_the_depth_gauge` |
| Poisoned locks on cold paths | Gateway books, conduit drafts and the cancel-reason guard used `if let Ok` / `.ok()`: the update was dropped, and the notify fired unlocked | `into_inner` recovery; the cancel notify stays under the guard | `tests/conduit.rs`, `tests/channel.rs` |
| `Protect` order | `abort()` took the resource before running the rollback, so the closure saw a freed value | Rollback first, resource second (same order as `Drop`) | `tests/guard.rs` (`error_drops_resource_and_executes_rollback_once`) |
| Drain ordering | `feed` checked EOF before taking the ingest lock (a concurrent close could interleave); the `ClockFn` ran under the state lock | Ingest lock taken before the EOF check; one clock read per chunk outside the state lock (`apply_token_at`) | `tests/drain_feed.rs` |
| Channel HWM / throttle | `capacity * 3 / 4` could overflow at absurd capacities; depth loaded `Relaxed` could bypass an armed throttle | `capacity - capacity / 4`; `Acquire` depth load | `tests/channel.rs` |
| Overflow / clamp | `ttl.as_millis() as u64` truncated on 128-bit `as_millis`; restart reserve could overflow; recovered lane histograms could exceed `in_flight`; `slowest_stage` indexed past `stage_count` | `try_from` saturating cast; `saturating_add`; lane clamp to the aggregate after recovery; clamped stage slice | `tests/profiling.rs`, `tests/node_stats.rs` |
| Example honesty | Examples 03/11/12 "proved" rollbacks with counters they incremented themselves; 04 slept without checking | Observable state (open DB handles, iron energization, laser latch) and a real timed sleep with an elapsed assertion | `examples/03_*.rs`, `examples/04_*.rs`, `examples/11_*.rs`, `examples/12_*.rs` |
| Bench hygiene | `TaskConduit::pair` / `PipeDrain::drain_sync` allocations escaped the HEAVY allowlist; `write_to` benches used `std::io::sink()` (no copy, so the number was fiction); per-sample drains were built inside the timed loop | HEAVY list extended; `sink()` banned; lane attach/reap use `iter_batched` so drain construction is untimed (`attach+reap` 338/407 → 220/177 ns, `write_to` 0.5 → 2.1 ns re-measured) | `tests/bench_hygiene.rs` |
| Script/CI teeth | grep rc=2 read as "no match"; missing reports and tool/target SKIPs passed; a baseline tag equal to `HEAD` compared the crate to itself; `cargo-semver-checks` (no `--locked` flag) could rewrite `Cargo.lock` unnoticed; the stress harness published a PASS even when its stats report failed to write | All are FAILs; `--locked` everywhere; both API gates hash `Cargo.lock` around the run; an empty/missing stats report aborts the harness; zero-sample leak gates fail | `scripts/*`, `.github/workflows/ci.yml` |
| Miri provenance | Fake wakers built via `transmute`-style tricks tripped strict provenance | `Waker::from(Arc<dyn Wake>)`; `-Zmiri-strict-provenance` in both CI lanes | `tests/task_engine.rs`, CI |
| Settle-path `mem::forget` | Layer-1 `commit`/`abort` settled with `mem::forget(self)`, which skips drop glue for every field — the exact shape that leaked one `NodeInner` per transaction from `TrackedGuard` before 0.5.2 | The event is constructed and the disarmed guard drops normally (`Drop` sees the taken slots); `#![deny(clippy::mem_forget)]` plus gate 7 reject any reintroduction | `tests/guard_alloc_budget.rs::layer1_settlement_is_byte_neutral_across_250k_guards` |
| Erased path allocation | `ErasedGuard` boxed the rollback and the commit hook separately, and the node path cloned the node twice and closed over it from two closures: two allocations and two `Arc` clones per admission | One boxed `ErasedSettle` transaction holds the rollback, the hook, the node clone and the optional token; settle, abort bookkeeping and panic semantics are unchanged | `tests/erased.rs`, `tests/supervisor.rs`, `tests/guard_alloc_budget.rs`; `enter_erased_auto + commit` 301.6 → 207.75 ns |

Documentation-only: README saturation figure restated as measured
(~1.1M admissions/sec on six contending threads), `WorkerPulse`
footprint arch-conditional (8 B / 4 B), pool and contention numbers
re-measured on this machine, storm timings re-measured (mixed storm
`wall_ms=474`, 2026-10-26 artifact
`docs/evidence/2026-10-26_pool_mixed_stress_report.json`; ~90k lease
samples), and the API table gains `PoolReap`,
`LaneSnapshot`, `PoolTotals`, `expired_lanes`, `DEFAULT_EOF_GRACE_MS`
and the argv-twin rows.

## Fixed in 0.6.7: sixth-audit batch

Conduit waker identity, core-exposed drain, honest bookkeeping, CI teeth.

| Fix | Contract before | Contract now | Test |
|---|---|---|---|
| `ResumeFuture` waker identity | Re-poll dedup used `will_wake`: two `ResumeFuture`s polled by one task cross-matched, so a pause directive could update the sibling's queue slot and orphan the other | Identity is a minted `Arc<AtomicBool>` flag per parked future (`Arc::ptr_eq`); re-park replaces the waker on its own slot only; `Drop` removes exactly that slot | `tests/conduit.rs::dropped_resume_future_removes_its_parked_waker` |
| Wake outside the wakers lock | `wake_async` called `waker.wake()` while holding the conduit's wakers lock: executor code ran under the lock, and a foreign panic poisoned it silently | Wakers are drained under the lock and woken after it; the lock is poison-tolerant (`into_inner`) | `tests/conduit.rs` |
| Async queue TTL origin (conduit sibling) | The spurious re-poll re-park re-stamped `enqueued_at_ms`, so a task that repeatedly re-polled could starve its own `task_ttl` forever | The TTL clock is stamped at the FIRST park only; re-parks refresh the waker, never the clock | `tests/task_engine.rs::spurious_repolls_do_not_reset_the_ttl_clock` |
| Memory-pressure pairing | `note_memory_kb` stored the pressure verdict with `Release`, but admission paths loaded it `Relaxed`: a pressure refusal could be decided from a stale verdict | Loads pair with the store (`Acquire`) on both sync and async admission paths | `tests/memory_boundary.rs` |
| `FaultKind` wire growth | The snapshot flag region grew silently with `FaultKind::COUNT`; a future kind could walk the flag bytes into the CRC region undetected | Compile-time const-assert: flag region + CRC must fit the snapshot contract, or the build fails | `src/diagnostics.rs` const gate |
| Core drain reachability | `ProcessToken` / `SnubScanner` / `TOKEN_*` / `OP_*` lived in the `std` supervisor module: bare-metal feed-driven parsing was impossible | New always-on `drain` module (core): token wire types, parser, and scanner compile under `no_std`; `supervisor` re-exports keep every historical path; `snubber::ProcessToken` / `snubber::SnubScanner` now resolve at the crate root | `tests/drain_feed.rs`, thumb gate |
| Honest refusal buckets | Four suites swallowed refusals with `Err(_)` or hand-counted one bucket: a miscounted or vanished attempt passed green | Every attempt lands in exactly one counted bucket and the local counts are tied to the node's own books (started / committed / implicit drops / bulkhead / breaker); unexpected refusal kinds panic | `tests/stress_breaker_churn.rs`, `tests/priority_preemption.rs`, `tests/transition_orphan.rs`, `tests/industrial.rs`, `tests/bulkhead_capacity.rs` |
| CI `--locked` | Most cargo invocations (clippy, no_std checks, doc, miri, bench compile) resolved the dependency graph unlocked: a stale `Cargo.lock` surfaced at release, not at PR | Every cargo invocation in CI runs `--locked` | `.github/workflows/ci.yml` |
| Semver-gate SKIP teeth | `check_api_diff` exited 0 on a missing tag baseline or missing tool — a misconfigured workflow skipped Gate 6 silently | Under `CI=true` a SKIP is a FAIL (both `.sh` and `.ps1` flavors); local runs still SKIP | `scripts/check_api_diff.sh`, `scripts/check_api_diff.ps1` |
| Bench snapshot honesty | `bench.sh` warned on an empty snapshot locally and wrote the output file non-atomically: a crashed parser could leave a truncated README table | Empty snapshot fails in every mode (parity with `bench.ps1`); the snapshot is published atomically (temp file + move) | `scripts/bench.sh` |
| Zero-sampling leak gates | Storm harnesses skipped the leak gates when the sampler collected 0 samples (e.g. its `catch{}` swallowed every poll): a run with no samples passed | Zero sampling in launch mode is an explicit FAIL check | `scripts/stress_harness.ps1`, `scripts/stress_harness.sh`, `scripts/stress_multi_lineage.ps1` |
| Explicit SKIP accounting | `stress_multi_lineage.py` booked the macOS-unavailable handle-growth check as a PASS ("n/a") | Platform-gated checks are reported as explicit SKIP rows and never count as passes | `scripts/stress_multi_lineage.py` |
| Windowed-batch wording | `failure_rate` was documented as a "sliding window", but the counters reset after every `window` outcomes: a failure pattern spanning a batch boundary is judged by its batch, and the docs overpromised | Docs (builder, stats fields, preset, USER_GUIDE) describe the filled-batch window honestly | doc sync (0.6.7) |

Documentation-only: the README `no_std`/`alloc` surface list now matches
the module gates (core `drain` added, `conduit`/`conflate` listed as
`std`, `erased`/`name` listed as `alloc`); the USER_GUIDE API table
gained `run_supervised_guard_with_grace` and the core `drain` rows and
cites the TTL regression test.
