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

---

## 2. Verification Protocol

The continuous integration suite validates these invariants under sustained chaos testing:

```bash
# Verify admission conservation under heavy concurrency (8 threads x 5,000 cycles)
cargo test --test concurrency_stress

# Verify process death handling and supervisor pipe recovery
cargo test --test supervisor
cargo test --test process_died

# Verify bulkhead pushback and rate limits
cargo test --test bulkhead_capacity
cargo test --test circuit_breaker
```

---

## 3. Reporting a Discrepancy

If you identify a scenario where any of the four invariants above fails using purely Safe Rust:

1. **Minimal Reproduction:** Construct an isolated integration test demonstrating the violation (failing an explicit `assert!`).
2. **Deterministic Trigger:** The test must run and fail within 30 seconds without requiring external infrastructure.
3. **Submission:** Open an issue or Pull Request titled `invariant-violation: <invariant_name>` containing the test case in `tests/`.

Confirmed defects will be patched immediately with a dedicated regression test named after the reporter or organization.
