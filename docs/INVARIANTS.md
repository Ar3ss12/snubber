# Mathematical Invariants & Verification Protocol

`snubber` is designed around strict deterministic guarantees. Unlike best-effort resilience wrappers, its core state machine enforces formal bounds on resource allocation, concurrency limits, and state transitions under abnormal termination (including panics, thread cancellation, and external process kills).

---

## 1. Core Invariants

The state machine of `snubber` guarantees that the following four properties hold unconditionally under Safe Rust:

### Invariant 1: Conservation of Admissions
At any point in time, the sum of all terminal and active states equals the total number of attempted admissions:

$$\text{Attempts} = \text{Admitted} + \text{BulkheadFull} + \text{BreakerOpen} + \text{StaleEpoch} + \text{RateLimited} + \text{InvariantBreach}$$

where `BulkheadFull` folds the async `QueueFull` verdict in (the queue is part of the bulkhead), and async `QueueTtlExpired` attempts settle as `Timeout` (a timed-out attempt, not a shed one). Memory-pressure refusals are counted **before** the perimeter in their own lock-free column and are therefore not part of the perimeter equation. No request is silently dropped, double-counted, or lost during state transitions, even under high-concurrency races.

### Invariant 2: Strict Slot Containment (Zero Leaks)
For any `SnubberNode` configured with capacity $C$:

$$0 \le \text{in\_flight}(t) \le C \quad \forall t$$

with one deliberate, documented exception: under `OverflowPolicy::EvictOldest` a saturated node overbooks by one ($C + 1$) to admit the newest caller; the displaced owner's slot is released by its own settlement (implicit drop → `EvictedOldest` column), so the overbooking is transient and never stacks. The $C + 1$ ceiling is enforced at admission (fixed in 0.6.4): a caller arriving while the overbook slot is already taken is refused with `BulkheadFull` — a second overbook would be erased from `in_flight` by the hard slot clamp while the live guard kept its real slot, desynchronizing `lane_in_flight` from `in_flight` and permanently disabling the Critical-over-Background preemption precondition ($\text{lane\_in\_flight}[0] = \text{in\_flight}$).

When all caller threads or async tasks complete (whether through `commit()`, explicit `abort()`, implicit RAII drop, or panic unwinding), the active lease count strictly returns to zero:

$$\lim_{t \to \infty} \text{in\_flight} = 0 \quad (\text{quiescent state})$$

No orphaned slot or leaked permit can permanently reduce node capacity.

### Invariant 3: Single-Shot Rollback Guarantee
For any acquired guard or lease:
- The registered rollback routine is invoked **at most once**.
- A successful `commit()` permanently disarms the rollback.
- An `abort()` or unwinding `Drop` triggers the rollback exactly once.
- Double-drop, nested unwinding, or re-entrant drops are mathematically unreachable.

### Invariant 4: Bounded Pipe Drain under Process Termination
In the multi-process supervisor (Layer 4), when a monitored child process terminates abnormally via `SIGKILL` or `TerminateProcess`:
- The supervisor guarantees reader termination within a strictly bounded window (`DEFAULT_EOF_GRACE_MS` = 250 ms; overridable per pool via `PoolConfig::with_eof_grace`).
- Pipe handles are closed without kernel deadlocks or thread starvation.
- The external reservation held on behalf of the child is released unconditionally.

### Invariant 5: Monotonic Hang Clock + Settled Spin Verdict
For any armed guard with a wall lease (`timeout_ms > 0`):
- The clock never moves backwards: NTP sync, DST change, or manual
  edits cannot un-expire a deadline or rewind `elapsed_ms`
  (boot-relative `Instant` on `std`, `MonotonicMs` tick on firmware —
  never `SystemTime`).
- `check_timeout` settles a blown wall lease exactly once (rollback runs
  once, `Timeout` policy fires once, slot returns once) with a verdict
  from pure `classify_stall` math: `SleepBlocked` when CPU stood still
  (syscall / `Condvar` / deadlocked `Mutex`), `CpuSpin` only when
  `cpu_delta >= cpu_budget_ms` (`0` = wall-only, never spin).

### Invariant 6: Zero-Nesting Locks (Deadlock Impossibility)
`SnubberNode`'s lock NEVER nests inside `TaskConduit`'s lock, and vice
versa. Cross-module calls happen only AFTER `drop(guard)` (open-call):
wakers fire outside the node lock, the conduit never arms guards.
Nested locks (ABBA) are the only remaining deadlock class, so the law
is enforced by `tests/lock_hierarchy.rs` as a grep-gate (doc-comments
stripped, code hits trip).

### Invariant 7: Pulse Lag Is Never Silent (Black-Box Trace)
Every missed `WorkerPulse` verdict recorded via
`SnubberNode::note_pulse_stall(lag_ms)` lands in the cold frame:
`SnapshotV1.pulse_stalls` / `max_pulse_lag_ms` (pulse fields since v3, 512 B kept) and
fires `AnomalyKind::PulseStall` (`pulse_stall`, ignores `min_sample`
like desync). A post-mortem dump answers "did it lag?" with
`[DIAG 0x… <node> pulse_stall <lag>%]` instead of "0 errors".

### Invariant 8: Memory Pressure Is a Refusal, Never a Fault
External RSS samples (`SnubberNode::note_memory_kb`, `MemoryPolicy::note`)
only move a `Copy` verdict; the crate never measures memory itself.
- Above `soft_kb` every new admission fails `EnterError::MemoryPressure`
  from ONE relaxed atomic load taken BEFORE the node mutex — a pressured
  node never contends with its sampler or a snapshotter, and in-flight
  guards keep running to settle (cooperative drain).
- The refusal has its own column (`memory_rejections`): it never enters
  the failure window, never moves `failure_streak`, and never trips the
  breaker. `retry_after_ms()` is a fixed 1000 ms (`Refusal::Backoff`).
- A hard kill exists only on the supervisor side, only at `used >= hard_kb`,
  and is stamped `DeathCause::MemoryLimit`; soft crossings never kill.
- `hard < soft` is clamped up to `soft`, so no configuration can skip the
  drain phase.
- `soft` is a level, not a deadline: `hard - soft` is the allocator's
  headroom, and the hard kill fires on the first sampler tick (10 ms) at or
  above `hard`. A checkpoint that allocates a second copy of the state does
  not get time to flush; it dies inside the allocation.
- `StateDigest` is a 64 B ticket, not storage. The crate guarantees one
  CRC32 over the 64 bytes and a 3-frame (24/24/16 B) transport where torn
  or mismatched frames are dropped; serialization, the fixed 64 KiB buffer,
  tmp + `sync_all` + `rename`, page-cache eviction (`posix_fadvise`) and
  validation of the file body are app-owned. A kill mid-write leaves no
  sealed file, hence no valid ticket.

### Invariant 9: Impedance Is Explicit (One Slot, Two Walls)
Mismatched producer/consumer rates are handled by named primitives, never
by an accidental unbounded queue.
- `ConflateSender` retains at most ONE unsampled value. A publish into an
  occupied slot replaces it and increments `conflated`; `publish` returns
  that fact. Over any window
  `published == taken + conflated + occupied`, where `occupied` is 0 or 1.
  The counter is a monotonic relaxed load, the payload carries no `Clone`
  bound, and a producer never blocks on the consumer.
- `ExecutionEnvelope` is a 16 B `Copy` header: min/max wall µs and min/max
  payload bytes. `0` disables one bound; `max < min` raises the ceiling to
  the floor; bounds are inclusive. The wall half is judged only for
  profiled guards (`work_wall_us`); the payload half is exact for any
  guard from the caller's byte count.
- A floor or ceiling miss ticks `envelope_underflows` /
  `envelope_overflows` and nothing else: it never feeds the breaker, never
  changes `SnubberEvent`, never enters the 512 B `SnapshotV1` frame, and
  never turns a committed transaction into a failure.
- The harness is deterministic: the same seed produces the same payload
  sequence, and an injected clock produces exact microseconds. A burst is
  a non-overlapping run of `burst_len` consecutive samples strictly above
  the median.

### Invariant 10: Complete Resource Reclaim on Settlement (Zero-Leak Gate)

Every settlement path — `commit()`, `abort()`, `check_timeout()`, or an
implicit `Drop` — MUST deallocate every owned guard field: the
`Arc<Mutex<NodeInner>>` reference, the captured rollback closure and any
boxed profile state. Disarming the rollback by calling `mem::forget(self)`
is an architectural defect: it suppresses drop glue for the whole struct,
so the `Arc` refcount never returns to 1 and the node's backing allocation
outlives every transaction.

- Settlement state lives in an internal `settled: bool` flag plus
  `Option::take()` on single-owner fields. No settlement path may bypass
  field destruction; the guard leaves scope normally and the compiler emits
  full drop glue.
- Net heap delta per settled transaction is exactly **0 bytes**: 100,000
  `enter_auto` → `commit` cycles followed by the node's drop must restore
  the allocator baseline (`tests/guard_alloc_budget.rs`). The same gate
  covers Layer 1: 200,000 `SnubberGuard` / `ErasedGuard` settle cycles
  whose closures own a heap payload restore the thread's heap baseline.
- Reintroducing one `mem::forget(self)` shows up as a stranded
  `NodeInner` allocation (2,288 B in the reference build), not as a
  statistic drift — and gate 7/7 plus `#![deny(clippy::mem_forget)]`
  reject the call before it builds.
- Closure ownership is observable: `Arc::strong_count` on a captured
  handle must return to 1 after commit, abort, timeout and implicit drop,
  and the admission slot must be reusable (`tests/guard_settle_reclaim.rs`).
- The out-of-process harness enforces the runtime ceiling: supervisor
  private-memory growth above `--MaxGrowthMb` / `--max-growth-mb`
  (default 2.0 MB) fails the run even when every token was rescued.
- The multi-lineage harness gates per-lane handle growth
  (`-MaxHandleGrowth` / `--max-handle-growth`, default 16) alongside private
  growth; first, peak and last samples are reported so a one-off warm-up
  delta is visible. Descriptor sampling reads `/proc/<pid>/fd` on Linux and
  `lsof -nP -p <pid> -F f` on macOS, so the gate applies on both runners.
  Where neither source exists the check is reported as a skip, and a skip
  fails the verdict: reduced coverage is never a pass.
- Every reap is classified: **true ambush** (armed token left at death),
  **clean escape** (a commit was seen), or **stillborn** (no commit and no
  armed token). `-ArmedHold` / `--armed-hold` publishes the worker pid only
  after the first arm frame, so `-MinAmbushPct 70` / `--min-ambush-pct 70`
  proves the killer terminates armed workers instead of shooting during
  process startup.
- The POSIX harness verifies process identity before `SIGKILL`: starttime
  plus image basename from `/proc/<pid>/stat` and `/proc/<pid>/exe` (Linux)
  or `ps -o lstart=,comm=` (macOS). A failed read is never cached, a killed
  pid drops its cached identity, and supervisor pids are excluded, so a
  recycled pid or a stale pid file cannot redirect a kill.
- In `WorkerPool`, every worker owns a private `PipeDrain`: a lane's
  `take_armed()` can only return a token emitted on that lane's pipe, so no
  rescue token crosses lanes.
- A lane's generation is bumped exactly once per reported death
  (`note_death` is the only bump site); no other lane's clock moves, and a
  reap hands the successor the post-bump generation.
- Pool conservation: `ambush + escape + stillborn + deadlocks == reaps` per
  lane and in the totals. `deadlocks` (no EOF inside `eof_grace_ms`) is a
  fourth honest bucket — never folded into the other three — and
  `PoolReport::healthy()` requires it to be zero.
- Restart math is per lane: one lane's crash storm (`GiveUp`) cannot stop,
  restart or delay another lane. The memory watch resets on `attach`, so a
  previous child's peak cannot euthanize its successor, and a reap or
  memory sample on an idle lane returns `NoWorkerAttached` instead of a
  silent zero.
- A lane lease (`expired(lease_ms)`) is measured from a frame's boot-relative
  timestamp; "no frame ever parsed" is tracked by a separate `seen` flag, so
  timestamp `0` (a frame parsed at process start) is not mistaken for
  silence. `hard < soft` clamps up, so the drain phase always exists.
- `WorkerPool::expired_lanes(lease_ms)` is a read-only sweep: it names a lane
  only when that lane's pipe is alive, was seen at least once, and has been
  silent past the lease. It takes each lane's state lock separately, never
  touches a child and never mutates counters, so a monitor thread can call it
  while every lane waiter is blocked in `child.wait()`.

### Invariant 11: Chunking-Independent Parsing (Feed Equivalence)
`PipeDrain::feed` is the same scanner as the reader-thread path, so frame
delivery cannot depend on how the transport sliced the stream:

- For any partition of the same byte stream, `feed` delivers the same
  sequence of `ProcessToken`s and the same `frames` / `arms` / `commits`
  counters as `drain_sync` over the whole stream.
- A partial frame is retained across calls: bytes are neither dropped nor
  duplicated at a chunk boundary. Bytes inside a 40-byte body are opaque
  (inserting garbage mutates that token, never starts a nested frame); the
  next `SNUB` magic re-synchronizes.
- `feed` returns exactly the number of frames completed by that call.
- `finish()` is the only EOF source for a manual drain: idempotent,
  thread-safe, discards a torn tail, and makes every later `feed` return
  zero without mutating state.
- The injected `ClockFn` is the only time source of a drain: `idle_ms`,
  `expired` and the pool EOF grace read the same counter, so a fake clock
  reproduces lease and deadlock verdicts without wall time.
- `ProcessToken::write_to` / `StateDigest::write_to` emit the identical
  bytes as `to_bytes()` / `to_frames()`, so a sink round-trip through
  `feed` yields the same counters and rescue classification as stdout.

### Invariant 12: Public API Extensibility Contract (0.6.0)

The public verdict surface evolves additively and honestly:

- Error/classification enums (`EnterError`, `FaultKind`, `AnomalyKind`,
  `ReapClass`, `RestartDecision`, `Directive`, `Refusal`, `GateError`,
  `MemoryVerdict`, `DeathCause`, `GatewaySendError`,
  `GatewayRecvError`, `PoolError`, `IsrPushError`, `StallKind`,
  `TimeVerdict`, `PayloadVerdict`, `SnapshotError`, `BreakerState`,
  `SnubberError`, `StageKind`, `TimeoutCheck`) are `#[non_exhaustive]`:
  a new variant is additive semver, and downstream matches must carry a
  `_` arm. Internal (defining-crate) matches stay exhaustive — the
  attribute binds only downstream crates, and a wildcard there would be
  unreachable code.
- Foreign hooks never run under the node lock. `friction_fn` /
  `friction_fn_ptr` / `pressure_fn` handlers are stored as
  `Arc<dyn Fn + Send + Sync>`, cloned (one atomic increment) under the
  lock and invoked after it is dropped; a handler may re-enter the node
  without deadlock.
- Node identity is dynamic without touching the `Copy` telemetry frames:
  `NodeName::{Static, Owned}` clones as a fat-pointer copy or one `Arc`
  increment; `SnapshotV1`, `MetricSample`, `Anomaly` and the wire
  headers keep `&'static str` (owned labels export the `<owned>`
  marker there).
- Format ceilings are published constants, not accidents:
  `Priority::COUNT` (3), `PROFILE_STAGES` (4), `ISR_RING_CAP` (16),
  `TOKEN_PAYLOAD_LEN` (28) / `TOKEN_BODY_LEN` (40), `SNAPSHOT_LEN`
  (512), `DEFAULT_EOF_GRACE_MS` (250). Each is compile-asserted in the
  source; changing one is a wire-format version bump.
- `Supervisor::..._argv(&[String])` variants run the supervisor loop on
  an explicit argument vector; without `--supervisor` or with the worker
  flag present they return `false` before spawning anything.

---

### Invariant 13: Parked-Waiter and Probe Liveness (0.6.1)
Every blocking mechanism in the crate must be WOKEN or DEREGISTERED, never
silently abandoned:
- Gateway depth: every successful send adds exactly 1, every take subtracts
  exactly 1 (signed arithmetic). After a full drain the counter is exactly 0
  and the throttle latch is clear — drift is impossible by construction.
- A parked `enter_async` waiter is woken by settlement OR removed by its
  future's `Drop`. The `EvictOldest` victim is woken and re-polls into
  `Err(EnterError::QueueFull)` — no zombie tasks, no wake/evict livelock.
- `check_timeout` on a half-open probe counts the timeout as probe failure:
  the breaker re-opens and the probe slot is released.
- `EpochGate` adopts a NEW generation unconditionally (epoch 0 included) —
  a rebooted client's first packet is never dropped.
- Conduit sync notifications (`resume`, `send_directive`) are issued while
  holding the `cancel_reason` mutex the waiter holds across its
  check-to-park window — a notification cannot land in the gap.
- A pool lane's generation bumps BEFORE its EOF grace wait: a successor
  attaching inside the window never inherits the dead child's generation.

Verification: `cargo test --test channel --test async_primitives --test circuit_breaker --test conduit --test supervisor_pool`

## 2. Verification Protocol

The continuous integration suite validates these invariants under sustained chaos testing:

```bash
# Verify admission conservation under heavy concurrency (8 threads x 5,000 cycles)
cargo test --test concurrency_stress

# Verify process death handling and supervisor pipe recovery
cargo test --test supervisor

# Verify bulkhead pushback and rate limits
cargo test --test bulkhead_capacity
cargo test --test circuit_breaker

# Verify monotonic hang layer (SPIN vs SLEEP + pulse)
cargo test --test clock_pulse

# Verify ISR bridge SPSC ring (no unsafe, M0-safe)
cargo test --lib isr

# Verify zero-nesting lock law (deadlock impossibility)
cargo test --test lock_hierarchy

# Verify pulse black-box (stall verdicts in the current-version snapshot + anomaly)
cargo test --test diagnostics pulse_stall

# Verify extensibility contract (hooks outside the lock, names, ceilings, argv)
cargo test --test extensibility

# Verify memory refusal + snapshot v5 gauges (soft refuses, hard verdicts)
cargo test --test memory_pressure

# Verify supervisor euthanasia decisions and the real soft-drain loop
cargo test --test supervisor_memory
cargo test --test supervisor_memory_loop

# Verify worker pool lanes (isolation, generation, conservation, deadlock grace)
cargo test --test supervisor_pool

# Verify the pool storm gate (4 lanes x 8 hard kills; ignored by default)
cargo test --test supervisor_pool_stress -- --ignored --nocapture

# Verify all four reap classes in one run (armed/clean/silent/orphan; ignored)
cargo test --test supervisor_pool_stress_mixed -- --ignored --nocapture

# Verify lease euthanasia of heartbeat-then-hang workers with a live observer
cargo test --test supervisor_pool_stress_lease -- --ignored --nocapture

# Verify chunked parsing equivalence, injected clocks and token sinks (invariant 11)
cargo test --test drain_feed

# Verify bench hygiene (setup outside the hot loop, pure overhead only)
cargo test --test bench_hygiene

# Verify one-slot conflation (latest wins, exact coalescing, payload drops)
cargo test --test conflation

# Verify envelope laws (zero disables, clamp, underflow/overflow own columns)
cargo test --test execution_envelope

# Verify harness determinism (scripted clock matrix, payload range, PRNG)
cargo test --test harness

# Verify zero-leak settlement: Arc/closure reclaim + byte-neutral 100k cycles
cargo test --test guard_settle_reclaim
cargo test --test guard_alloc_budget

# Verify out-of-process kill path under armed-hold (true ambush >= 70%)
# Windows (PowerShell); POSIX equivalents: scripts/stress_harness.sh --launch --aggressive
# and scripts/stress_multi_lineage.py
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 1000 -Launch -RawKill -Aggressive -ArmedHold

# Verify N independent lineages under a parallel death storm (Windows, 4 x 2,500 kills)
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold

# Same storm on Linux/macOS (SIGKILL path, run by .github/workflows/stress.yml)
python3 scripts/stress_multi_lineage.py --lines 4 --kills-per-line 2500 --aggressive --armed-hold
```

---

## 3. Reporting a Discrepancy

If you identify a scenario where any of the thirteen invariants above fails using purely Safe Rust:

1. **Minimal Reproduction:** Construct an isolated integration test demonstrating the violation (failing an explicit `assert!`).
2. **Deterministic Trigger:** The test must run and fail within 30 seconds without requiring external infrastructure.
3. **Submission:** Open an issue or Pull Request titled `invariant-violation: <invariant_name>` containing the test case in `tests/`.

Confirmed defects will be patched immediately with a dedicated regression test named after the reporter or organization.
