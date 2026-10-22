# Mathematical Invariants & Verification Protocol

`snubber` is designed around strict deterministic guarantees. Unlike best-effort resilience wrappers, its core state machine enforces formal bounds on resource allocation, concurrency limits, and state transitions under abnormal termination (including panics, thread cancellation, and external process kills).

---

## 1. Core Invariants

The state machine of `snubber` guarantees that the following four properties hold unconditionally under Safe Rust:

### Invariant 1: Conservation of Admissions
At any point in time, the sum of all terminal and active states equals the total number of attempted admissions:

$$\text{Attempts} = \text{Admitted} + \text{BulkheadFull} + \text{BreakerOpen} + \text{StaleEpoch} + \text{RateLimited}$$

No request is silently dropped, double-counted, or lost during state transitions, even under high-concurrency races.

### Invariant 2: Strict Slot Containment (Zero Leaks)
For any `SnubberNode` configured with capacity $C$:

$$0 \le \text{in\_flight}(t) \le C \quad \forall t$$

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
- The supervisor guarantees reader termination within a strictly bounded window ($t \le 100\text{ ms}$).
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
`SnapshotV1.pulse_stalls` / `max_pulse_lag_ms` (v3, 512 B kept) and
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
  the allocator baseline (`tests/guard_alloc_budget.rs`).
- Reintroducing one `mem::forget(self)` shows up as a stranded
  `NodeInner` allocation (2,288 B in the reference build), not as a
  statistic drift.
- Closure ownership is observable: `Arc::strong_count` on a captured
  handle must return to 1 after commit, abort, timeout and implicit drop,
  and the admission slot must be reusable (`tests/guard_settle_reclaim.rs`).
- The out-of-process harness enforces the runtime ceiling: supervisor
  private-memory growth above `--MaxGrowthMb` / `--max-growth-mb`
  (default 2.0 MB) fails the run even when every token was rescued.

---

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

# Verify pulse black-box (stall verdicts in v3 snapshot + anomaly)
cargo test --test diagnostics pulse_stall

# Verify memory refusal + snapshot v4 gauges (soft refuses, hard verdicts)
cargo test --test memory_pressure

# Verify supervisor euthanasia decisions and the real soft-drain loop
cargo test --test supervisor_memory
cargo test --test supervisor_memory_loop

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
```

---

## 3. Reporting a Discrepancy

If you identify a scenario where any of the ten invariants above fails using purely Safe Rust:

1. **Minimal Reproduction:** Construct an isolated integration test demonstrating the violation (failing an explicit `assert!`).
2. **Deterministic Trigger:** The test must run and fail within 30 seconds without requiring external infrastructure.
3. **Submission:** Open an issue or Pull Request titled `invariant-violation: <invariant_name>` containing the test case in `tests/`.

Confirmed defects will be patched immediately with a dedicated regression test named after the reporter or organization.
