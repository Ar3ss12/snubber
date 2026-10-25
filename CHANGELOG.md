# Changelog

All notable changes to `snubber`. Benchmarks live in [README.md](README.md);
the deeper contract tables live in [docs/USER_GUIDE.md](docs/USER_GUIDE.md)
("Fixed in …" sections) and [docs/INVARIANTS.md](docs/INVARIANTS.md).

## Unreleased

Eighth-audit batch (hook panics, scanner reset order, lock hygiene, honest
tests and benches, CI teeth). No API removals; `Cargo.toml` still reads
0.6.8 until a release is ordered.

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
