# Changelog

All notable changes to `snubber`. Benchmarks live in [README.md](README.md);
the deeper contract tables live in [docs/USER_GUIDE.md](docs/USER_GUIDE.md)
("Fixed in …" sections) and [docs/INVARIANTS.md](docs/INVARIANTS.md).

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
