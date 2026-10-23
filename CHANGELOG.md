# Changelog

All notable changes to `snubber`. Benchmarks live in [README.md](README.md);
the deeper contract tables live in [docs/USER_GUIDE.md](docs/USER_GUIDE.md)
("Fixed in …" sections) and [docs/INVARIANTS.md](docs/INVARIANTS.md).

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
