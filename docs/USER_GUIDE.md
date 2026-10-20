# snubber — user guide

> `snubber` is a transactional safety fuse for Rust. It guarantees that
> if your function fails — through a panic, a `?` early return, or an
> early `return` — your hardware, database transactions, or memory resources
> automatically reset to a safe state through the `Drop` destructor.
>
> Zero dependencies, no `unsafe`, works from bare-metal firmware
> (`core`-only) to high-throughput desktop and server daemons (`std`). MSRV 1.70.
>
> Hang layer (v0.5): monotonic `Instant` clock (no `SystemTime` jumps),
> CPU-spin classifier (`TimedOut` vs `SpinDetected` vs `DeadlockSuspected`,
> silence ceiling optional), 8-byte `WorkerPulse`
> heartbeat. See [Layer 1.5](#layer-15-hang-classification-spin-vs-sleep-vs-deadlock--pulse).

---

## Quick Start: Zero-Boilerplate Scoped Execution

If you don't want to manually handle `guard.commit()` or track tokens in your head, use the **`scoped`** functional API. It turns transactional safety into a single clean call. Its sealed sibling **`scoped_sealed`** adds a `!Send` `ScopeToken` (thread-leak fails at compile), **`transition_checked`** + `LegalMove` close the state graph (illegal transitions rejected at compile time), **`StaticNode`** validates capacities FIRST in `build()`, and **`Protected`** releases or rolls back the resource with a guard — all opt-in, see the API table:


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

// 2. Admission boundary gate + Scoped work: validates invariants BEFORE arming hardware
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
* `SnubberError::Admission(EnterError)` — The node rejected the call at the admission perimeter (circuit breaker open, bulkhead full, rate limited, invariant breach). **Protected resources were never touched; no rollback executed.**
* `SnubberError::Work(E)` — The protected work unit returned an error `Err(E)`. **Emergency rollback was automatically executed via RAII before returning.**

---

## Layer 1: Core RAII Guard (Stack Rollback)

No nodes, no config. Three lines:

```rust
use snubber::SnubberGuard;

let guard = SnubberGuard::new(epoch, task_id, || {
    hardware::force_release(); // runs automatically on failure or panic
});

risky_operation()?; // `?` or panic here → hardware released
guard.commit();     // success: disarm the rollback
```

How it works: the guard owns your rollback closure. `commit()` disarms it (success path). If the scope exits any other way, `Drop` runs the closure. This relies on stack unwinding — the normal Rust panic and `?` path — so it costs virtually nothing (~1.5 ns on AMD Ryzen 5 4600H) when nothing fails.

### Rules for the rollback closure:
1. **Safe to run twice:** Releasing a button twice is fine; sending money twice is not — put terminal forward actions in `commit()`.
2. **Fast and non-blocking:** Atomic stores, event flags, non-blocking sends.
3. **No node re-entrancy:** Never call back into the same node's admission method under the rollback (that deadlocks).
4. **Memory ordering:** The node's mutex protects node books only. Every guard runs a `SeqCst` fence before the rollback body (free on x86/x64) — but a fence pairs with NOTHING. If your rollback publishes to shared state that another thread reads, pair it yourself: store with `Release`, load with `Acquire` (or guard the state with your own lock). `Relaxed`×2 reads stale data (F13).
5. **Bind resource to guard:** Wrap the handle in `Protected::new(res, guard)` — `commit()` disarms the guard and returns the resource; any other exit executes rollback and drops the resource. Post-crash access becomes a compile error, not a forgotten `if`.
6. **Seal the scope:** Prefer `node.scoped_sealed(rb, work)` where thread-leakage is the threat — the `!Send` `ScopeToken` makes `spawn` with context a compile error (E0277).

---

## Layer 1.5: Hang Classification (SPIN vs SLEEP vs DEADLOCK + Pulse)

Wall-clock timeouts cannot tell a hot `loop {}` spin from a thread
blocked in a syscall: both blow the wall lease. The hang layer adds a
second signal — thread CPU time — and a third circuit: a silence ceiling
that names an eternal deadlock instead of an eternal `SleepBlocked`. An
8-byte heartbeat completes the layer. All math is
`core` + `Copy` (`src/clock.rs`, no `unsafe`, no `std`); OS backends
(`GetThreadTimes`, `clock_gettime`, `DWT_CYCCNT`) live in YOUR app.

### Clock: monotonic, never `SystemTime`

`std` time comes from boot-relative `Instant` (`SystemClock`): NTP
sync, DST changes, and manual edits never move deadlines. Firmware
passes its own tick (`SysTick` / cycle counter) via `MonotonicMs`.

### Classifier: `classify_stall(wall, cpu, wall_budget, cpu_budget)` / `classify_stall_ceiling(.., stall_ceiling)`

| `wall_delta` | `cpu_delta` | ceiling | Verdict | Meaning |
|---|---|---|---|---|
| `<= wall_budget` | any | — | `WithinBudget` | keep working |
| `> wall_budget`, `cpu_budget == 0` | any | below | `SleepBlocked` | wall-only mode: blown lease, short pause — tolerated |
| `> wall_budget` | `< cpu_budget` | below | `SleepBlocked` | blocked in syscall / `Condvar` / driver / brief EDR pause — do NOT treat as spin |
| `> wall_budget` | `>= cpu_budget` | any | `CpuSpin` | hot spin — de-energize iron past the thread, never kill the thread |
| `> wall_budget` | `< cpu_budget` | `>= ceil` | `DeadlockStall` | wedged lock / dead pipe / over-long suspension (ceiling = default `0` disables) |

### Guard wiring: CPU budget + sampler

```rust
use snubber::{SnubberNode, StallKind, TimeoutCheck};

static CPU_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn thread_cpu_ms() -> u64 {
    // App backend: GetThreadTimes / clock_gettime / DWT_CYCCNT delta.
    CPU_MS.load(std::sync::atomic::Ordering::SeqCst)
}

let node = SnubberNode::builder("laser")
    .timeout_ms(100)      // wall lease
    .cpu_budget_ms(50)    // CPU lease: 0 = wall-only (default)
    .stall_ceiling_ms(2000) // silence ceiling: 0 disables (default); suggested wall_budget*20
    .build();
let guard = node.enter(1, 1, || laser_off())?.with_cpu_sampler(thread_cpu_ms);

// Non-destructive probe (no settle):
assert!(matches!(guard.stall_kind(), StallKind::WithinBudget));

// Cooperative checkpoint — settle is IDENTICAL for all three causes,
// only the verdict differs:
match guard.check_timeout() {
    Ok(g) => { /* within budget, keep working */ }
    Err(TimeoutCheck::TimedOut) => { /* sleep/block under ceiling — rollback RAN */ }
    Err(TimeoutCheck::SpinDetected) => { /* hot spin — rollback RAN */ }
    Err(TimeoutCheck::DeadlockSuspected) => { /* silence past ceiling — rollback RAN */ }
}
```

`with_cpu_sampler` takes a plain `fn() -> u64` (no closure, no alloc,
no lock); the arming sample is captured at attach time so deltas are
measured from arming, not from boot.

### Heartbeat: `WorkerPulse` (8 bytes) + `PulseWatch`

```rust
use snubber::{PulseWatch, WorkerPulse};

let pulse = WorkerPulse::new();          // worker side: 8 B
let mut watch = PulseWatch::new(2000);   // watcher side: 2 s lease
// worker loop: pulse.kick();            // one Release store / iter
// watcher tick: watch.observe(pulse.tick(), wall_now_ms)
//   true  = tick advanced or lease intact
//   false = tick frozen past lease → de-energize iron + report Timeout
```

Contract: the watcher MUST NOT kill the worker thread or drop its
guard from outside (poisoned mutexes, leaked allocator state). On a
missed pulse it drives iron to safe state past the thread and reports
via `node.note_pulse_stall(lag_ms)` — the lag lands in `SnapshotV1`
(v3: `pulse_stalls` + `max_pulse_lag_ms`) and fires
`AnomalyKind::PulseStall`, so the post-mortem dump answers lag with
`[DIAG 0x… <node> pulse_stall <lag>%]`. The worker settles itself at
its next `check_timeout`.
On firmware the same `kick()` strokes the hardware WDT (`WdtSink`;
`NoWdt` is the `std` no-op). See `examples/11_hang_classifier.rs`.

### ISR bridge: `IsrBridge` (66 B SPSC ring, no `unsafe`)

A hardware ISR cannot take a mutex, sleep, or call `enter()`. It
pushes a packed `IsrSignal` (`opcode:8 | reason:8 | value:16` in one
`AtomicU32`); the main loop pops and steers. `head`/`tail` are
`AtomicU8` — M0-safe byte atomics, no CAS, no `cpsid i`.

```rust
use snubber::{IsrBridge, IsrSignal, OP_EMERGENCY_TRIP, OP_PULSE};

static BRIDGE: IsrBridge = IsrBridge::new();
// ISR: BRIDGE.try_push(IsrSignal::new(OP_EMERGENCY_TRIP, sensor_id, reading));
// main loop:
while let Some(signal) = BRIDGE.try_pop() {
    match signal.opcode() {
        OP_EMERGENCY_TRIP => { guard.abort("hardware_isr_trip"); break; }
        OP_PULSE => pulse.kick(),
        _ => {}
    }
}
```

`try_push` fails `Full` instead of blocking or evicting — an unread
trip is never overwritten. See `examples/12_isr_bridge.rs`.

### Idle park: `IdlePark` (`Condvar` on `std`, `wfi` on firmware)

`park()` names the 0%-CPU sleep: the `std` worker already parks on a
`Condvar` (`TaskConduit::wait_while_paused`); firmware overrides
`IdlePark::park` with `wfi` (core stops its clock until the next
interrupt — the `unsafe` asm lives in YOUR app, never here).
`SpinPark` is the default for bare `core` targets.

---

## Layer 2: Concurrency Bulkhead (Bounded Queues & Backpressure)

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
        // Bad input rejected during parameter validation before touching hardware — fix the caller.
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

### 2.5 Long-Lived Worker Daemon (Autonomous Slot Claiming)

Consider a concurrency-limited execution perimeter where **only ONE operation
may execute concurrently** (`max_in_flight(1)`). Any incoming concurrent admission
while the slot is held is rejected at the bulkhead boundary with:
`EnterError::BulkheadFull`.

### The trap: wrapping `thread::spawn` in `scoped_checked`

A developer thinks: *"I need to start the background worker thread safely — I'll
wrap the launch in `scoped_checked`!"*

```rust
// Caller thread (UI or the "Start" command)
click_node.scoped_checked(&params, validate, || {}, |_| {
    // 1. Caller thread claimed the slot: in_flight = 1. Capacity exhausted.

    std::thread::spawn(move || {
        // 2. The newborn worker thread attempts admission on the SAME node:
        let _guard = click_node.enter_erased_auto(..)?;
        // ❌ REJECTED: The node observes in_flight == 1 and returns BulkheadFull!
        // The worker never started — it aborts immediately.
    });

    // 3. Only after exiting this closure is the slot released (in_flight = 0).
})?;
```

Millisecond physics of the race condition:


1. **ms 0:** the main thread calls `scoped_checked`. Snubber opens the
   door, leads it in, locks the gate. The only free slot is taken.
2. **ms 1:** the main thread tells the OS: `thread::spawn` (make me a
   background thread).
3. **ms 2:** while the main thread has not left its braces yet, the
   newborn clicker thread starts and makes its first call:
   `click_node.enter_erased_auto()`.
4. **ms 3:** the node looks at its counter: limit 1, occupied 1. **It
   bounces the worker off the wall.** The worker panics or quietly closes
   with an error.
5. **ms 4:** the main thread finally finishes the launch, the
   `scoped_checked` destructor fires, the slot frees up… but the worker
   is already dead.

You shut the door in the face of the one you just called in.

### Does `scoped` free the slot? YES — and that is exactly the problem

`scoped` lifecycle:

```text
node.scoped(|| {
    // 1. [ENTER]: snubber takes the slot. in_flight becomes 1.

    do_some_work(); // 2. Your code runs.

    // 3. [EXIT]: the closing brace }. Destructor fires: slot back to 0.
});
```

The moment the thread leaves the closure, the slot is **instantly free**
— panic included, via `Drop`. Locking the door is the bulkhead's direct
duty: without it, `max_in_flight(1)` would be an empty word, and two
threads would shred the protected resource.

But `std::thread::spawn` **does not wait** for the worker to finish
clicking — it asks the kernel to create a thread and returns in ~0.1 ms.
So the outer `scoped` guards the 0.1 ms of *creating* the thread, not
the *hours* of its work:

* worker wakes up in the first 0.1 ms → hits the locked door → dies;
* worker wakes up at 0.2 ms → door is open, but then why was `scoped`
  there at all? You guarded the spawn call, never the click loop.

### The iron law: WHO DOES THE WORK TAKES THE SLOT

The function that merely creates the thread (`thread::spawn`) must not
touch the node's slots — it is just a dispatcher. The slot is taken by
**the worker itself from the inside**, once awake in the OS:

```rust
// Main thread just spawns, holding NO snubber slots:
std::thread::spawn(move || {
    // The worker woke up and FIRST takes the single slot for its whole life:
    let guard = match click_node.enter_erased_auto(|| emergency_release_all_hardware()) {
        Ok(g) => g,
        Err(_) => {
            // We land here ONLY if the user really spammed "Start" and a
            // second worker is already running in parallel.
            return;
        }
    };

    // ... the click loop spins for minutes/hours ...

    guard.commit(); // clean exit, slot freed
});
```

### Canonical recipe: `enter_erased_auto` + commit in the epilogue

```rust
use snubber::{ErasedGuard, SnubberNode};

struct ClickDaemon {
    // Nameable guard: lives in the struct field across awaits and loop
    // iterations. A generic `TrackedGuard<F>` cannot do this — every
    // closure is a different anonymous type.
    hold: Option<ErasedGuard>,
}

fn spawn_worker(node: &SnubberNode) -> Result<(), snubber::EnterError> {
    // 1. Arm at spawn. Rollback = emergency hardware release.
    let guard = node.enter_erased_auto(|| hardware::release_all())?;
    // 2. Run the loop (clicks, polls, waits — whatever the daemon does).
    // 3. Settle EXACTLY once in the epilogue:
    //    clean stop  → guard.commit()   (disarm, no rollback)
    //    panic / `?` → Drop runs the rollback automatically
    guard.commit();
    Ok(())
}
```

Rules:
1. **One guard per run, settled once.** Never arm per loop iteration —
   the flight recorder would drown in `total_started`.
2. **External stop signals the worker; the worker settles itself.**
   The stop button sets a flag / signals an event; the thread observes
   it, breaks the loop, and calls `commit()` on its own stack (where the
   `MouseHoldGuard` still owns the hardware).
3. **Validate BEFORE arming.** Run invariant checks first; on breach call
   `node.report(FaultKind::PoisonedPayload, epoch, task_id)` and start
   nothing — `total_started` stays untouched, no slot burned.

### Where `scoped` DOES belong (short stack-local transactions)

`scoped` is built **exclusively for short transactions on this very
thread (current stack, under ~50 ms)**:

```rust
// Saving config to disk: took the slot → wrote in 2 ms → freed it.
config_node.scoped(|| save_settings_to_file())?;
// Updating shared coordinates: 5 µs — in, update, out.
state_node.scoped(|| update_shared_memory())?;
```

Moral, one line each:
* **Short action right here (≤ 50 ms)?** → `scoped`. It enters, works,
  frees on exit by itself.
* **Long background spinner (worker, timer, daemon)?** → no `scoped`.
  Use **`spawn_guarded`** — thread + guard + leash in one call, so you
  never wire them wrong by hand:

```rust
use snubber::{Directive, SnubberNode, TaskConduit};

let node = SnubberNode::builder("click_worker").max_in_flight(1).build();
// ONE call binds all three: the thread, its guard, its leash.
let (join, handle) = node.spawn_guarded(
    || hardware::release_all(), // rollback if the worker dies
    0u64,                       // initial conduit state
    |conduit: TaskConduit<u64, ()>| {
        // The worker armed enter_erased_auto FIRST (lane Normal),
        // publishes liveness, parks cooperatively on directives:
        loop {
            match conduit.check_directive() {
                Directive::Cancel(_) | Directive::Preempted(_) => break,
                Directive::Pause => {
                    let _ = conduit.wait_for_resume();
                }
                _ => do_click(),
            }
        }
        // Exit → guard.commit() automatically.
    },
);
// The controller steers ONLY through the leash — never touches slots:
handle.send_directive(Directive::Cancel("stop"));
join.join().expect("worker").expect("admitted"); // Err(Admission) = wall said no
```

### Priority lanes: who yields when the senior knocks (Phase 1)

Three lanes, ordered by rank (`Background(0) < Normal(1) < Critical(2)`).
Default is `Normal` — every existing `enter*` call keeps its meaning:

```rust
use snubber::{Priority, SnubberNode};

let node = SnubberNode::builder("clicker").max_in_flight(1).build();

// Low-priority task holds the single slot on the Background lane:
let bg = node.enter_with_priority(Priority::Background, 1, 1, || release_all())?;

// Emergency stop knocks on the senior lane. Over pure-Background
// occupancy the node files ONE cooperative `Preempted` request
// (flag + counter, lock released!) and refuses as `BulkheadFull` —
// the caller retries after its victims yield:
match node.enter_with_priority(Priority::Critical, 2, 2, || release_all()) {
    Ok(stop) => {
        stop.commit();
    }
    Err(snubber::EnterError::BulkheadFull) if node.preempt_requested() => {
        // Tell the background worker through its own conduit, then retry:
        // handle.send_directive(Directive::Preempted("e-stop"));
        // ... victim observes check_directive() -> abort("preempted") ...
        // let stop = node.enter_with_priority(Priority::Critical, 3, 3, ...)?;
    }
    Err(other) => return Err(other.into()),
}
```

Strict rule (no inversion): `Critical` preempts ONLY pure-`Background`
occupancy (`lane_in_flight == [n, 0, 0]`). `Critical` vs `Normal` never
preempts — plain `BulkheadFull`, no flag, `preempted` column stays clean.
The node never forces (R5): who never checkpoints `check_directive()`
is its own enemy — the request waits until the worker yields itself.
Diagnostics: `node.preempt_requested()`, `node.lane_in_flight()`,
`stats().preempted` / `shed_preempted()` (own column, operational shedding, never
trips the breaker, never counted in `admission_total`).

Never stand in the turnstile doorway while calling someone in. If you
walked in yourself — nobody else gets through.

---

## Layer 3: Runtime-Agnostic Async (Tokio, Axum, gRPC)

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
    Err(Refusal::Backoff) => {
        // Wall pressure — sleep retry_after_ms() in YOUR runtime,
        // then return HTTP 429 Too Many Requests with Retry-After header.
    }
    Err(Refusal::FailFast) => {
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

## Layer 2 Subsystem: Bi-directional Task Steering (`TaskConduit` & `TaskHandle`)

Traditional functions run blindly once invoked. **Bi-directional Task Steering** (`TaskConduit` and `TaskHandle`) provides zero-dependency, two-way intercom between the invoker and the worker:

```rust
use snubber::{Directive, TaskConduit};

let (conduit, handle) = TaskConduit::pair(42);

// In worker task:
while work_remains() {
    // Check for steering directives (Proceed/Pause/Cancel/SkipStage).
    // check_directive() returns a plain Directive (never an Option).
    match conduit.check_directive() {
        Directive::Pause => {
            conduit.wait_for_resume_async().await; // 0% CPU async parking via Waker
        }
        Directive::Cancel(reason) => return Err(reason),
        Directive::SkipStage => continue,
        Directive::Proceed => {}
    }
    conduit.update_state(|s| s.stage += 1);
    conduit.publish_draft(intermediate_result); // Shared operating table
}

// In controller task / UI:
let live_state = handle.inspect(); // Lock-free live telemetry inspection
handle.send_directive(Directive::Pause);
let draft = handle.take_draft(); // Take intermediate progress without canceling
handle.resume(); // Proceed == resume (there is no Directive::Resume)
```

---

## Layer 4: Process Supervisor & Crash Recovery (Surviving `kill -9`)

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

### Out-of-Process Hard Termination Harness (10,000 OS Kills):
The architecture has been verified against **10,000 real Windows `TerminateProcess` executions** at ~52 kills/sec without a single pipe deadlock or lost token:
```powershell
# Run the automated 10,000 kill stress test:
cargo test --test supervisor_termination -- --ignored --nocapture
```



### 4.5 Declarative restart (Phase 2: Declarative process restart tracking)
The supervisor does not guess — it counts. Declare the policy once,
the caller loop owns spawn/sleep:
```rust
use snubber::{Backoff, RestartDecision, RestartPolicy, RestartTracker, RestartWindow};

let mut tracker = RestartTracker::new(
    RestartPolicy::OnCrash,          // Never | OnCrash | Always
    Backoff::exponential(50, 1000),  // 50→100→200…ms cooling, capped
    RestartWindow::new(3, 10),       // max 3 restarts per 10 s
);
loop {
    let code = spawn_and_wait(); // caller owns the process
    match tracker.note_death(code, now_ms()) {
        RestartDecision::Restart { delay_ms, .. } => caller_sleep(delay_ms),
        RestartDecision::GiveUp { .. } => {
            node.report(snubber::FaultKind::ProcessDied, 1, 1);
            break; // storm: park the lineage, operator decides
        }
        RestartDecision::Stop => break, // Never, or clean exit(0)
    }
}
```

### 4.6 Hot state handoff (Phase 3)
64 B state snapshot (`StateDigest`, one cache line, opaque bytes):
child process publishes via `OP_STATE` frames, drain keeps the last complete frame,
replacement process reads it on tick 0 — process restores where it died, not default.

> **Explicit packing (deterministic serialization):** pack field-by-field
> with `to_le_bytes` into the 64 B array — never `transmute`, never raw-pointer
> casts. Padding bytes carry uninitialized memory into the pipe; explicit
> packing leaves no gaps by construction:
```rust
use snubber::StateDigest;
fn pack_state(x: i32, y: i32, flags: u32, clicks: u64) -> StateDigest {
    let mut b = [0u8; 64]; // zeroed first — no garbage, ever
    b[0..4].copy_from_slice(&x.to_le_bytes());
    b[4..8].copy_from_slice(&y.to_le_bytes());
    b[8..12].copy_from_slice(&flags.to_le_bytes());
    b[12..20].copy_from_slice(&clicks.to_le_bytes());
    // bytes 20..64 stay zero until the next field earns them
    StateDigest::new(b)
}
```
```rust
use snubber::StateDigest;
// Child process: pack flat (or a storage ticket for 100 MB on disk)…
let digest = StateDigest::new(packed_64_bytes);
digest.emit_if_supervised(); // no-op when unsupervised
// …death… supervisor hands it to the replacement process stdin tick 0:
```
Chain closed 100% in-crate (debt 2): the parent side pipes stdin,
writes 64 B (fits any 4 KB kernel buffer — never blocks), drops for
EOF; the replacement process reads on its first line:
```rust
use snubber::{StateDigest, Supervisor};
// Parent: caller builds Command (exe+args+env), crate pipes + writes:
let mut child = Supervisor::spawn_with_digest(cmd, &digest)?;
// Replacement process, first line of main (ONCE — second call sees EOF):
if let Some(digest) = snubber::take_stdin_digest() {
    restore_from_bytes(digest.bytes); // 42 is back, state restored
}
```

### 4.7 Orphan contract (Phase 6: no zombies)
```rust
use snubber::{NoOrphanGuard, OrphanGuard};
let guard = NoOrphanGuard; // honest no-op; real Win32 Job Objects implemented in app
assert!(!guard.arm(child_pid));
```

---

## Layer 5: Diagnostics & Health (Catastrophic Faults vs Capacity Shedding + `app.snub`)

A node that absorbs hits but tells nobody is an opaque black box. `snubber`
splits every fault into two isolated contours — and gives you the tools
to see operational backpressure **before** it becomes an unrecoverable failure.

### The two contours

| Contour | Meaning | Kinds | Breaker |
|---|---|---|---|
| **Catastrophic Fault** (Unrecoverable) | The task died anomalously — algorithmic defect | `Panic`, `Timeout`, `ProcessDied`, `ImplicitDrop` | Feeds the streak, trips breaker |
| **Capacity Shedding** (Operational Backpressure) | The healthy node holds the perimeter — tune capacity | `BulkheadFull`, `BufferOverflow`, `EvictedOldest`, `StaleEpoch`, `BreakerOpen`, `RateLimited`, `ChannelDisconnected`, `PoisonedPayload` | Counted, never trips breaker |

Single source of truth: `FaultKind::is_catastrophic()`. Diagnostic counters on
every snapshot: `stats().catastrophic_faults()`, `shed_bulkhead()`,
`shed_rate()` (own column forever — slow worker vs outer spam),
`shed_stale()`, `admission_total()`.

### Filter 1 — CI gate (stop the skew before release)

```rust
use snubber::assert_no_anomalies;

#[test]
fn scheduler_under_load_stays_clean() {
    run_stress_workload(&node);
    // Iron rule: the test fails red on any anomaly verdict.
    assert_no_anomalies!(&node);
}
```

### Filter 2 — Diagnostics Console (Telemetry Pulse)

```rust
use snubber::{AnomalyPolicy, print_health};

let snap = node.snapshot_v1(); // cold 512 B snapshot, cached
let policy = AnomalyPolicy::default(); // 15% bottleneck / 30% flood / min 100
print_health("click_worker", &snap, &policy);
// [SNUBBER PULSE] node="click_worker"
// CRITICAL: 0 breaches
//   bulk_path  | calls: 124000 | shed_bulk: 0 (0%)   | OK
//   spam_path  | calls: 124000 | shed_rate: 142 (0%) | OK
//   stale_path | calls: 124000 | shed_stale: 0       | OK
```

### Filter 3 — production (`app.snub` + one-line diagnosis)

The crate never touches the filesystem — your app owns the flusher
thread (e.g. `snapshot_v1_in(gen)` → `encode()` → tmp + rename every 10 s →
`decode()` + `find_anomalies()` on boot). When a user reports *"it lags"*, ask for one line:

```text
[DIAG 0x8FA3 click_worker bulkhead_starvation 92%]
```

That line is `diagnostic_line(node, anomaly, &snap)` — a FNV-1a hash
over the counters plus the verdict. No screenshots, no guessing.

### Multi-node set (explicit, never a `static`)

```rust
use snubber::NodeSet;

let mut set = NodeSet::new();
set.register("click_worker", &click_node);
set.register("hotkey_listener", &hotkey_node);
for anomaly in set.find_all_anomalies(&AnomalyPolicy::default()) {
    eprintln!("{}", snubber::diagnostic_line(anomaly.node, &anomaly, &snap));
}
```

Cold paths only: snapshot rarely and cache; never call `stats()` per
CPS in a hot loop (F10 — the `last_fault` clone shows up in the
profiler at 160 CPS).

---

## Multiplatform POSIX / Linux & Embedded Integration

The crate maintains `#![forbid(unsafe_code)]` and zero external dependencies. Platform-specific system calls (`prctl`, `pidfd`, `clock_gettime`) belong in application code, behind the clean traits provided by `snubber`.

### 1. No-Zombie Law: Application-Side `OrphanGuard` Implementations

When a supervisor process terminates abnormally, child worker processes must not become orphaned zombies:

| Target Platform | Kernel Mechanism | App-Side Implementation Pattern |
|---|---|---|
| **Windows** | Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) | Assign child process handle to a Job Object configured to terminate all processes on handle closure. |
| **Linux (POSIX)** | `prctl(PR_SET_PDEATHSIG, SIGKILL)` | In child process (pre-exec or line 1 of child `main`): `libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL)`. |
| **Linux 5.3+** | `pidfd_open` / `pidfd_send_signal` | Parent monitors child via pidfd without PID recycling races. |
| **Container / k8s** | Subreaper / init daemon | Use `NoOrphanGuard` (reaping owned by container init PID 1 / dumb-init / tini). |

#### Linux `OrphanGuard` Recipe:
```rust,ignore
pub struct LinuxPdeathsigGuard;

impl snubber::OrphanGuard for LinuxPdeathsigGuard {
    fn arm(&self, _pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == 0
        }
        #[cfg(not(target_os = "linux"))]
        false
    }

    fn backend_name(&self) -> &'static str {
        "linux-prctl-pdeathsig"
    }
}
```

### 2. CPU Time Samplers for Hang Classification

`with_cpu_sampler(sampler: fn() -> u64)` accepts a raw monotonic tick counter. The crate calculates differential math in pure `core`:

| Target Platform | Syscall / Instruction | App-Side Sampler Implementation |
|---|---|---|
| **Windows** | `GetThreadTimes` | Returns `(KernelTime + UserTime) / 10_000` (ms). |
| **Linux (glibc / musl)** | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` | Returns `ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000`. |
| **ARM Cortex-M (no_std)** | `DWT->CYCCNT` | Read cycle counter directly: `DWT::cycle_count() / (CPU_FREQ_HZ / 1000)`. |

---

## API & Cost Quick Reference

| Operation | Layer | Latency / Footprint | When to Use |
|---|---|---|---|
| `snub_check!(cond, reason)` | 1 / core | ~0 ns (single branch) | Universal invariant check, returns `Err` |
| `SnubberGuard::new` | 1 / core | **~1.5 ns** (stack move) | Single function synchronous rollback |
| `EpochGate::observe` | 1 / core | **~2.6 ns** | Monotonic ordering & reboot re-handshake |
| `classify_stall(wall, cpu, wb, cb)` / `classify_stall_ceiling(.., ceiling)` | 1 / core | const math, 0 ns | SPIN vs SLEEP vs DEADLOCK verdict from one sample pair (+ silence ceiling) |
| `WorkerPulse::kick` + `PulseWatch::observe` | 1 / core | 8 B, 1 atomic store | Heartbeat: missed pulse = de-energize past thread, never kill it |
| `SystemClock` (`MonotonicMs`) | 1 / `std` | 1 `Instant` read | Boot-relative monotonic ms; NTP/DST edits never move deadlines |
| `node.cpu_budget_ms(n)` + `node.stall_ceiling_ms(n)` + `guard.with_cpu_sampler(f)` | 2 / std | 1 fn call at arming | CPU-spin budget + silence ceiling; `check_timeout` settles spin/deadlock distinctly |
| `guard.stall_kind()` | 2 / std | 1 clock + 1 sample | Non-destructive hang verdict (no settle) |
| `guard.check_timeout()` | 2 / std | 1 clock + 1 sample | Cooperative watchdog: `WithinBudget` / `TimedOut` / `SpinDetected` / `DeadlockSuspected` |
| `IsrBridge::try_push/pop` + `trip/take_trips` + `IsrSignal` | 1 / core | 80 B, ~2 bus cycles | ISR signal into main loop; trip latch bypasses a full ring; no mutex/alloc/`unsafe` (M0-safe byte atomics); `head`/`tail` separated by 64 B cells, no `align(64)` pads |
| `IsrSignal::new(opc, reason, value)` | 1 / core | 4 B, const | Pack `OP_EMERGENCY_TRIP` / `OP_PULSE` / `OP_PAUSE` |
| `IdlePark::park` (`SpinPark`/firmware `wfi`/`wfe`) | 1 / core | 0% CPU | 0%-CPU park: `Condvar` on `std`, `wfi`/`wfe` impl on firmware (`wfe` needs `SEVONPEND=1` at boot) |
| `WdtSink::feed` / `NoWdt` | 1 / core | no-op on std | Stroke HW WDT on firmware (same `kick()` feeds it) |
| `node.note_pulse_stall(lag_ms)` | 2 / std | 1 atomic max | Record missed pulse into `SnapshotV1` v3 + `AnomalyKind::PulseStall` |
| `node.transition(&mut s, next, rb, work)` | 2 / std | **~86.5 ns** | ONE enum state machine, no flag drift |
| `node.transition_checked(s, rb, work)` + `LegalMove` | 2 / std | same + 0 ns | Closed graphs: illegal edges rejected at compile (E0277) |
| `node.enter_with_priority(prio)` | 2 / std | **~73.6 ns** | Critical over Background yields cooperatively |
| `Protected::new(res, guard)` + `commit()` | 1 / core | 0 ns (generics) | Resource burns with guard unless committed |
| `node.scoped_sealed(rb, work)` + `ScopeToken` | 2 / std | same as `scoped` | `!Send` seal: thread-leak fails at compile |
| `StaticNode::<I, Q>::build(name)` | 2 / std | same as builder | Capacities validated FIRST in `build()` |
| `gateway::<T>(cap)` + `host.recv()` | 2 / std | ~10 ns | Multiple senders, one receiver channel; HWM/LWM throttle paces at 75%/50% with integer `retry_after_ms`, `send_critical` bypasses |
| `RestartTracker::note_death(code, now)` | 2 / std | pure math | Process respawn backoff tracker, caller sleeps |
| `StateDigest` + `take_state()` | 2 / std | 64 B, ~1 ns | Hot handoff: memory survives death |
| `NoOrphanGuard` / `OrphanGuard` | 2 / std | trait only | Parent death = child processes reaped (app impl) |
| `node.enter_auto` | 2 / std | **~86.5 ns** | Monotonic auto sequence ticket minting |
| `contended_node_enter` (6 threads, shared `enter_auto` allocator) | 2 / std contended | **~452 ns** | MESI cache-line bouncing on node mutex; StaleEpoch losers bounded-retry (cap 64 + yield), same contract as `concurrency_stress` |
| `contended_node_scoped` (6 threads) | 2 / std contended | **~804 ns** | Full transactional cycle under contention (entry + work + auto-commit) |
| `contended_gateway_mpsc` (6 producers + drain) | 2 / std contended | **~132 ns** | Bounded multi-producer write under parallel core flood |
| `cross_thread_isr_bridge` (push core + pop core) | 1 / core inter-core | **~35.6 ns** | Lock-free SPSC ring across physical cores (honest inter-core bus latency, not L1-hot loop) |
| `node.enter_async_auto` | 2 / std | 0% CPU waker park | Async admission under bounded concurrency |
| `guard.settle_async(fut)` | 2 / std | zero lock during await | Commit across `.await` points |
| `TaskConduit` / `TaskHandle` | 2 / std | lock-free atomics | 2-way live steering, pause/resume waker, drafts |
| `PipeDrain` + `ProcessToken` | 2 / std | 44-byte stream | Hardware rollback across process death (`kill -9`) |
| `node.snapshot_v1()` + `encode()` | 2 / cold | 512 B, CRC32 | Crash-safe persistence, app-owned flush thread |
| `find_anomalies()` / `assert_no_anomalies!` | 2 / cold | pure `u128` math | CI gate + diagnostics console + `[DIAG 0x…]` bug reports |
| `NodeSet` set | 2 / cold | explicit `&Node` list + saturating aggregates | Multi-node pulse, never a `static` |
