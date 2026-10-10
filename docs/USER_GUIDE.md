# snubber — user guide

> `snubber` is a transactional safety fuse for Rust. It guarantees that
> if your function fails — through a panic, a `?` early return, or an
> early `return` — your hardware, database transactions, or memory resources
> automatically reset to a safe state through the `Drop` destructor.
>
> Zero dependencies, no `unsafe`, works from bare-metal firmware
> (`core`-only) to high-throughput desktop and server daemons (`std`). MSRV 1.70.

---

## 0. Universal Ergonomics: Zero-Boilerplate Scoped Execution

If you don't want to manually handle `guard.commit()` or track tokens in your head, use the **`scoped`** functional API. It turns transactional safety into a single clean call:

```rust
use snubber::{snub_check, SnubberNode};

let node = SnubberNode::builder("orders").build();

// 1. One call: runs work, commits automatically on Ok, rolls back on Err or panic
let tx_id = node.scoped(
    || db.rollback_transaction(), // emergency rollback if work fails
    || {                          // protected work unit
        wal.write_entry(&record)?;
        fsync_file()?;
        Ok::<u64, DbError>(record.id)
    }
)?;

// 2. Customs gate + Scoped work: validates invariants BEFORE arming hardware
let receipt = node.scoped_checked(
    &payment_request,
    |req| {
        snub_check!(req.cents > 0, "order amount must be positive");
        snub_check!(req.account_id != 0, "account id cannot be zero");
        Ok(())
    },
    || ledger.unlock_funds(),      // rollback if gateway charges fail
    |req| gateway.charge(req),     // protected work unit
)?;
```

### Error Model: `SnubberError<E>`
`scoped` returns `Result<T, SnubberError<E>>`, cleanly separating:
* `SnubberError::Admission(EnterError)` — The node rejected the call at the outer membrane (circuit breaker open, bulkhead full, rate limited, invariant breach). **Protected resources were never touched; no rollback executed.**
* `SnubberError::Work(E)` — The protected work unit returned an error `Err(E)`. **Emergency rollback was automatically executed via RAII before returning.**

---

## Level 1: Rollback for a single function (zero setup)

No nodes, no config. Three lines:

```rust
use snubber::SnubberGuard;

let guard = SnubberGuard::new(epoch, task_id, || {
    hardware::force_release(); // runs automatically on failure or panic
});

risky_operation()?; // `?` or panic here → hardware released
guard.commit();     // success: disarm the rollback
```

How it works: the guard owns your rollback closure. `commit()` disarms it (success path). If the scope exits any other way, `Drop` runs the closure. This relies on stack unwinding — the normal Rust panic and `?` path — so it costs virtually nothing (~0.51 ns) when nothing fails.

### Rules for the rollback closure:
1. **Safe to run twice:** Releasing a button twice is fine; sending money twice is not — put terminal forward actions in `commit()`.
2. **Fast and non-blocking:** Atomic stores, event flags, non-blocking sends.
3. **No node re-entrancy:** Never call back into the same node's admission method under the rollback (that deadlocks).
4. **Memory ordering:** The node's mutex protects node books only. If your rollback publishes to shared state that another thread reads, pair it yourself: store with `Release`, load with `Acquire` (or guard the state with your own lock).

---

## Level 2: Protecting a Daemon (Queues, Failures, Backpressure)

Wrap long-lived services, drivers, and background workers in a `SnubberNode`. The node is a bounded bulkhead with a flight recorder: it counts every path, remembers the last fault with caller file/line coordinates, and stops hammering dead hardware.

```rust
use snubber::{EnterError, SnubberNode};

let node = SnubberNode::builder("payment_gateway")
    .max_in_flight(16)      // at most 16 concurrent executions
    .trip_after_failures(3) // 3 crashes in a row → open circuit breaker
    .build();

match node.enter_auto(|| release_locks()) {
    Ok(guard) => {
        process_transaction()?;
        guard.commit();
    }
    Err(EnterError::BreakerOpen) => {
        // Hardware is sick — serve fallback value, keep painting UI / answering cache.
    }
    Err(EnterError::BulkheadFull) => {
        // All slots busy — back off by advised retry_after_ms(), then retry.
    }
    Err(EnterError::StaleEpoch) => {
        // Late or reordered packet from the past — skip safely.
    }
    Err(EnterError::RateLimited) => {
        // Over the per-second rate budget — slow down.
    }
    Err(EnterError::InvariantBreach(reason)) => {
        // Bad input rejected at membrane before touching hardware — fix the caller.
    }
    Err(EnterError::QueueTtlExpired) => {
        // Task waited in queue longer than task_ttl lease — fail fast.
    }
    Err(EnterError::QueueFull) => {
        // Waiter buffer capacity reached under RejectNew.
    }
    Err(EnterError::PayloadBudgetExceeded(bytes)) => {
        // Inbound payload exceeds byte budget — drop before allocation.
    }
}
```

### Presets
- `SnubberNode::desktop(name)` — single-flight (`max_in_flight: 1`) with breaker, optimized for GUI apps.
- `SnubberNode::embedded(name)` — tight and quiet, zero noise, for embedded/firmware.
- `SnubberNode::server(name)` — wide bulkhead plus sliding-window breaker for backend daemons.

---

## Level 3: Async Architecture (Tokio, Axum, gRPC)

`snubber` is completely runtime-agnostic: it requires **zero external dependencies** and uses pure `core::task::Waker` parking. It works identically in Tokio, async-std, smol, or bare-metal executors.

### 1. Zero-Runtime Waker Queue (`enter_async` / `enter_async_auto`)
When `max_in_flight` is reached, `enter_async_auto` parks the caller's `core::task::Waker` at **0% CPU** without running background polling threads or spinning:

```rust
let guard = node.enter_async_auto(|| rollback_transaction()).await?;
do_async_io().await?;
guard.commit();
```

### 2. Async Settle Across `.await` (`settle_async`)
`TrackedGuard` holds **NO mutex across `.await` points**. The lock is held for only ~80 nanoseconds during admission and completion:

```rust
// Runs the future across .await, commits if Ok, aborts and rolls back if Err:
let response = guard.settle_async(async {
    client.post_payment().await
}).await?;
```

### 3. Pacing Without Freezing Worker Threads (`enter_paced_async`)
Never call blocking sleeps (`std::thread::sleep`) in async code (Rule F14). Inject your runtime's timer:

```rust
let guard = node.enter_paced_async(epoch, task, rollback, |wait_ms| {
    tokio::time::sleep(std::time::Duration::from_millis(wait_ms))
}).await?;
```

### 4. Cancellation-Resistant Stage Checkpoints (`checkpoint` / `state_snapshot`)
When `tokio::select!` drops a future mid-work, standard guards lose context. `snubber` preserves the exact milestone reached:

```rust
let mut guard = node.enter_auto(|| rollback_partial_state())?;

step_connect().await?;
guard.checkpoint(1, "connected");

step_send_headers().await?;
guard.checkpoint(2, "headers_sent");

step_read_body().await?; // If dropped here by select! or timeout:
guard.commit();
```

Upon cancellation:
* `FaultRecord` captures `stage: 2`, `stage_label: Some("headers_sent")`, `queue_wait_ms`, `elapsed_ms`.
* `guard.state_snapshot()` provides an atomic, lock-free copy of the running transaction's live telemetry.
* `guard.is_expired()` checks watchdog deadlines non-destructively.

### 5. Task Engine & Queue Bounds
When building high-throughput pipelines, configure bounded waiter buffers and lifetime leases:

```rust
let node = SnubberNode::builder("task_pipeline")
    .max_in_flight(16)                             // concurrent execution slots
    .queue_capacity(64)                            // bounded waker queue buffer
    .task_ttl(std::time::Duration::from_millis(50)) // tasks waiting longer than 50ms fail fast
    .shed_policy(OverflowPolicy::RejectNew)        // or OverflowPolicy::EvictOldest
    .payload_budget(1024 * 1024)                   // 1MB max payload cutoff
    .queue_storage(StorageKind::InMemory)          // zero-dep waker buffer
    .build();
```

* **`task_ttl`**: Tasks waiting longer than their lease fail fast with `Err(EnterError::QueueTtlExpired)`.
* **`queue_capacity` & `shed_policy`**: At capacity, `RejectNew` returns `Err(EnterError::QueueFull)`, while `EvictOldest` discards the oldest waiter.
* **`payload_budget`**: Rejects oversized inputs before memory allocation (`Err(EnterError::PayloadBudgetExceeded)`).

### 6. Bounded Reaper Channel (`AsyncLease` / `LeaseChannel`)
When guards drop during task completion or cancellation, cleanups can be dispatched to background reaper tasks through bounded channels without blocking the worker thread:
```rust
let (tx, rx) = LeaseChannel::pair(256);
// Non-blocking try_send; channel overflow is recorded as FaultKind::BufferOverflow
```

### 7. Zero-Sleep HTTP/gRPC Checkpoint (`AsyncGate`)
For web frameworks (Axum, Actix, Tonic) where sleeping in request handlers is forbidden:
```rust
use snubber::{AsyncGate, Refusal};

let gate = AsyncGate::new(&node);
match gate.admit_auto(|| rollback()) {
    Ok(guard) => { /* process request */ guard.commit(); }
    Err(Refusal::Backoff { retry_after_ms, depth, kind }) => {
        // Return HTTP 429 Too Many Requests with Retry-After header
    }
    Err(Refusal::FailFast(kind)) => {
        // Circuit breaker open -> HTTP 503 Service Unavailable / fallback
    }
    Err(Refusal::BadRequest(reason)) => {
        // Invariant breached -> HTTP 400 Bad Request
    }
    Err(Refusal::Ignore) => {
        // Stale epoch -> HTTP 200 OK (silent discard)
    }
}
```

---

## Level 4: Cockpit Protocol (2-Way Live Intercom)

Traditional functions run blindly once invoked. The **Cockpit Protocol** (`TaskConduit` and `TaskHandle`) provides zero-dependency, two-way intercom between the invoker and the worker:

```rust
use snubber::{Directive, TaskConduit};

let (conduit, handle) = TaskConduit::pair(42);

// In worker task:
while work_remains() {
    // Check for steering directives (Pause, Resume, Cancel, SkipStage)
    if let Some(Directive::Pause) = conduit.check_directive() {
        conduit.wait_for_resume_async().await; // 0% CPU async parking via Waker
    }
    conduit.update_state(|s| s.stage += 1);
    conduit.publish_draft(intermediate_result); // Shared operating table
}

// In controller task / UI:
let live_state = handle.inspect(); // Lock-free live telemetry inspection
handle.send_directive(Directive::Pause);
let draft = handle.take_draft(); // Take intermediate progress without canceling
handle.send_directive(Directive::Resume);
```

---

## Level 5: Surviving Process Death (Supervisor & Framed Tokens)

When a process is vaporized instantly (`kill -9`, `Stop-Process -Force`, `std::process::abort()`, SIGKILL, or hung Win32 drivers), **no destructors (`Drop`) execute in that process**. Rescue must come from an out-of-process supervisor.

`snubber` provides a turnkey process supervisor with 44-byte framed pipe intercom:

1. **Generation Clock (`GenerationClock`):** The supervisor starts workers with `gen=N`. If the worker dies, the supervisor increments to `gen=N+1`. Any delayed zombie packets from the dead worker are cut off at the gate.
2. **Framed Pipe Intercom (`ProcessToken`):** The worker emits framed 44-byte packets (`b"SNUB"` sync magic + 40 bytes payload).
3. **PipeDrain:** The supervisor drains the stream continuously in a background reader with sliding-window sync, filtering out arbitrary `println!` noise.
4. **Emergency Rescue on EOF:** If the worker is killed mid-transaction while holding an armed token, the pipe closes (`EOF`), and the supervisor immediately executes the registered emergency hardware release outside the dead process.

### Single-Line Supervised Wire-Up:
```rust
// Worker side: one line links token arming, stdout pipe emit, and rollback
let guard = node.enter_supervised_erased(
    ProcessToken::simple(0, OP_HARDWARE_RELEASE, tag),
    move || { hardware::release_relay(relay_id); }
)?;
```

### Chaos Monkey Verification (10,000 OS Kills):
The architecture has been verified against **10,000 real Windows `TerminateProcess` executions** at ~52 kills/sec without a single pipe deadlock or lost token:
```powershell
# Run the automated 10,000 kill chaos stand:
cargo test --test chaos_dual_terminal -- --ignored --nocapture
```

---

## API & Cost Quick Reference

| Operation | Floor | Latency / Footprint | When to Use |
|---|---|---|---|
| `snub_check!(cond, reason)` | 1 / core | ~0 ns (single branch) | Universal invariant check, returns `Err` |
| `SnubberGuard::new` | 1 / core | **~0.51 ns** (stack move) | Single function synchronous rollback |
| `EpochGate::observe` | 1 / core | **~2.57 ns** | Monotonic ordering & reboot re-handshake |
| `node.scoped(rollback, work)` | 2 / std | ~80 ns | Automatic commit on `Ok`, rollback on `Err`/panic |
| `node.scoped_checked(...)` | 2 / std | ~84 ns | Customs gate + scoped transaction execution |
| `node.enter_auto` | 2 / std | **~84.8 ns** | Monotonic auto sequence ticket minting |
| `node.enter_async_auto` | 2 / std | 0% CPU waker park | Async admission under bounded concurrency |
| `guard.settle_async(fut)` | 2 / std | zero lock during await | Commit across `.await` points |
| `TaskConduit` / `TaskHandle` | 2 / std | lock-free atomics | 2-way live steering, pause/resume waker, drafts |
| `PipeDrain` + `ProcessToken` | 2 / std | 44-byte stream | Hardware rollback across process death (`kill -9`) |
