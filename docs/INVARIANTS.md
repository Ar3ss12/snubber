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

# Verify bench hygiene (setup outside the hot loop, pure overhead only)
cargo test --test bench_hygiene
```

---

## 3. Reporting a Discrepancy

If you identify a scenario where any of the seven invariants above fails using purely Safe Rust:

1. **Minimal Reproduction:** Construct an isolated integration test demonstrating the violation (failing an explicit `assert!`).
2. **Deterministic Trigger:** The test must run and fail within 30 seconds without requiring external infrastructure.
3. **Submission:** Open an issue or Pull Request titled `invariant-violation: <invariant_name>` containing the test case in `tests/`.

Confirmed defects will be patched immediately with a dedicated regression test named after the reporter or organization.
