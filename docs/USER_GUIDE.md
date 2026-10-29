# snubber — user guide

> `snubber` is a transactional safety fuse for Rust. It guarantees that
> if your function fails — through a panic, a `?` early return, or an
> early `return` — your hardware, database transactions, or memory resources
> automatically reset to a safe state through the `Drop` destructor.
>
> Zero dependencies, no `unsafe`, works from bare-metal firmware
> (`core`-only) to high-throughput desktop and server daemons (`std`). MSRV 1.70
> for the library; the tests and benches need a newer toolchain, because the
> dev-dependency `criterion` resolves to a `clap` that requires Rust 1.74. The
> `msrv` CI lane (`cargo check --lib`, core / core+alloc / std) is what proves
> the floor — a `cargo test` on 1.70 is not a supported configuration.
>
> This guide describes the current behavior of the crate.

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

### Symptom → chapter (quick diagnosis)

Five symptoms cover most production incidents; each row links the
chapter that owns the fix.

| Symptom | Where to look |
|---|---|
| The worker dies on `BulkheadFull` right after `thread::spawn` inside `scoped` | [2.5 Long-Lived Worker Daemon](#25-long-lived-worker-daemon-autonomous-slot-claiming) — who does the work takes the slot |
| The soft-limit worker is OOM-killed while streaming its checkpoint | [4.6c Checkpoint physics](#46c-checkpoint-physics-64-b-is-a-ticket-not-storage) — [streaming recipe](#streaming-recipe-bounded-heap) and [cgroup v2 page cache](#cgroup-v2-page-cache-counts-against-the-limit) |
| An async waiter never wakes or the queue wedges | [Layer 3](#layer-3-runtime-agnostic-async-tokio-axum-grpc) — waker queue bounds, `task_ttl`, `settle_async` |
| A dead child's pipe never reaches EOF (`ReapClass::Deadlocked`) | [4.6d Worker pool](#46d-worker-pool-workerpool-n-lineages-one-pipe-per-worker) — the fourth honest bucket and `pool.expired_lanes` |
| The snapshot reports anomalies or counters drift | [Layer 5](#layer-5-diagnostics--health-catastrophic-faults-vs-capacity-shedding--appsnub) — triage card, `assert_no_anomalies!` |

---

## Layer 1: Core RAII Guard (Stack Rollback)

Rollback is structural: an admitted guard reverts unless it is committed.

No nodes, no config. Three lines:

```rust
use snubber::SnubberGuard;

let guard = SnubberGuard::new(epoch, task_id, || {
    hardware::force_release(); // runs automatically on failure or panic
});

risky_operation()?; // `?` or panic here → hardware released
guard.commit();     // success: disarm the rollback
```

How it works: the guard owns your rollback closure. `commit()` disarms it (success path). If the scope exits any other way, `Drop` runs the closure. This relies on stack unwinding — the normal Rust panic and `?` path — so it costs single-digit nanoseconds when nothing fails (including the drop of every owned field; measured figures: [BENCHMARKS.md](../BENCHMARKS.md)).

### Rules for the rollback closure:
1. **Safe to run twice:** Releasing a button twice is fine; sending money twice is not — put terminal forward actions in `commit()`.
2. **Fast and non-blocking:** Atomic stores, event flags, non-blocking sends.
3. **Re-entrancy is allowed, not free:** the rollback body runs *outside* the node lock (Invariant 6, open-call), so calling back into the same node's admission from it does **not** deadlock — measured in `tests/rollback_guardrails.rs`. What actually happens: `scoped`/`scoped_checked` re-admission succeeds while a slot is free and returns `SnubberError::Admission(EnterError::BulkheadFull)` when the node is at capacity, because **your own rollback still holds its slot until it returns**; the async path returns `Poll::Pending` and wakes only when the body finishes. So a retry loop inside a rollback is a livelock of your thread, not a hang of the node. What is forbidden: re-locking a mutex that `work()` already holds (same thread → real hang, and the node books no verdict on its own — an outside `watchdog::poll_rollback_node` names the still-active body), and assuming the erased path will refuse you — `probe_erased_auto` releases admission at arm time, so nested erased scopes are granted by design.
4. **Memory ordering:** The node's mutex protects node books only. Every guard runs a `SeqCst` fence before the rollback body (free on x86/x64) — but a fence pairs with NOTHING. If your rollback publishes to shared state that another thread reads, pair it yourself: store with `Release`, load with `Acquire` (or guard the state with your own lock). `Relaxed`×2 reads stale data.
5. **Bind resource to guard:** Wrap the handle in `Protected::new(res, guard)` — `commit()` disarms the guard and returns the resource; any other exit executes rollback and drops the resource. Post-crash access becomes a compile error, not a forgotten `if`.
6. **Seal the scope:** Prefer `node.scoped_sealed(rb, work)` where thread-leakage is the threat — the `!Send` `ScopeToken` makes `spawn` with context a compile error (E0277).
7. **Never suppress guard drop:** Never use `std::mem::forget` on guards. A guard owns node references and telemetry allocations. Disarming is done strictly through `commit()`. `commit()` marks the transaction settled while allowing the guard's inner fields to cleanly release their allocations upon leaving scope.

### Watching a rollback from outside

The crate cannot cancel a rollback body (that needs `unsafe`, and no library
has the right to kill your thread), but it can name one — from another
thread, without the node lock:

```rust
let phase = node.rollback_phase(); // lock-free, no allocation
if phase.active && phase.elapsed_ms > my_budget_ms {
    // your reaction: alarm, drain, restart the worker
}
```

`watchdog::poll_rollback_node(node)` packages that read into a typed verdict
for the "it never returned" case — still no lock, no thread, no allocation,
and it books nothing. The threshold is the budget the node was built with,
so a watcher cannot time a body against a number the node never armed:

```rust
use snubber::watchdog::poll_rollback_node;
if let Some(stall) = poll_rollback_node(&node) {
    // stall.epoch / stall.task_id / stall.elapsed_ms / stall.budget_ms
}
```

You choose where to poll from (a supervisor tick, a `main` loop) and how
often. The elapsed clock belongs to the node, so `rollback_budget_ms(..)`
must be armed as well; otherwise `elapsed_ms` stays `0` and the function
returns `None` instead of inventing age. When your observer's threshold
must differ from the builder's (an external SLA, a test that has to see
the body sooner), the 0.6-shaped `poll_rollback(&node, my_budget_ms)` still
takes it explicitly — it can be stricter than the builder, but it can no
more manufacture age than the node form.

**What to do instead of re-locking the foreign mutex.** Queue the
compensating action (a lock-free send, an atomic flag) and let the thread
that owns the lock apply it after your body returns. If the lock is truly
required, use `try_lock()` and report the miss instead of waiting. The
crate does not cancel your body and does not kill your thread: the kill
switch belongs to your supervisor (`Supervisor`), and the in-process
alternative is this verdict. Worked example:
`examples/20_rollback_budget.rs` (finishes while its worker stays wedged).

* `active`, `epoch`, `task_id` and `reentries` always work: two atomic
  writes per body, no clock read, nothing to configure.
* `elapsed_ms`, `max_ms` and `stalls` need a budget:
  `SnubberNode::builder(..).rollback_budget_ms(200)`. Until one is armed
  they stay `0` — the default keeps the abort path clock-free.
* An overrun is a NAMED fault, not just a counter: a body that returns
  after its budget lands in `stats().reported[FaultKind::RollbackStall]`,
  in `last_fault`, in the flight ring, and through the default policy
  (`Warn`, metric tag `rollback_stalls_total`). A body that panics mints
  no verdict — an unwind is not a measurement.
* The bulkhead slot is held while `active` is true, so a `BulkheadFull`
  refusal that looks unexplained has its cause named here.
* Honest limits: guards you build by hand (`SnubberGuard::new`,
  `ErasedGuard::new`) carry no node handle and are invisible; a body that
  waits on a foreign lock shows as `active` forever — that is the finding,
  not a gap. Reference: `tests/rollback_guardrails.rs`.

### Re-entrant admission from a rollback body is a policy

`RollbackReentrancy::Allow` (default) keeps 0.6.x behavior byte-for-byte:
an admission attempted on the same thread that is running this node's
rollback body competes normally (slot free → admitted, slot held →
`BulkheadFull`) and is counted in `RollbackPhase::reentries`.
`RollbackReentrancy::Refuse` (opt-in) refuses that attempt *before any
lock* with `EnterError::RollbackReentrant` — other threads are never
affected, because the marker is per-thread:

```rust
let node = SnubberNode::builder("daemon")
    .rollback_reentrancy(RollbackReentrancy::Refuse) // paranoid mode
    .rollback_budget_ms(200)
    .build();
```

`RollbackReentrant` is a caller mistake, not backpressure: `classify()`
maps it to `Refusal::BadRequest` and `backpressure()` returns `None`, so
a retry from inside the same body repeats the mistake. The marker needs
thread identity, so the policy is inert on `no_std` (documented on the
enum); `rollback_phase().reentries` stays the only bucket for refused
attempts.

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

When the reading needs captured state — a thread handle, a worker's own
counter — attach a shared closure instead. It is the same contract and
the same arming law, but the closure owns its context:

```rust
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

let worker_cpu_ms = Arc::new(AtomicU64::new(0));
let sampler = {
    let worker_cpu_ms = Arc::clone(&worker_cpu_ms);
    move || worker_cpu_ms.load(Ordering::Relaxed)
};
let guard = node.enter(1, 1, || laser_off())?.with_cpu_sampler_shared(Arc::new(sampler));
```

`with_cpu_sampler_shared` is the only sampler path that allocates (the
`Arc` you pass is the allocation); the guard stores one fat pointer and
`TrackedGuard<fn()>` stays 320 B. The closure is typed
`dyn Fn() -> u64 + Send + Sync + UnwindSafe + RefUnwindSafe`, so the
guard keeps the auto traits it had before 0.7.0 (a bare `dyn Fn` would
strip them). When both samplers are attached, the shared one wins.
`node` is a `std` module, so `core`/`alloc` builds do not see this
method at all.

### Heartbeat: `WorkerPulse` (8 B, 4 B without 64-bit CAS) + `PulseWatch`

```rust
use snubber::{PulseWatch, WorkerPulse};

let pulse = WorkerPulse::new();          // worker side: 8 B (4 B without 64-bit CAS)
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
(pulse fields since v3; the frame is now v6) and fires
`AnomalyKind::PulseStall`, so the post-mortem dump answers lag with
`[DIAG 0x… <node> pulse_stall <lag>%]`. The worker settles itself at
its next `check_timeout`.
On firmware the same `kick()` strokes the hardware WDT (`WdtSink`;
`NoWdt` is the `std` no-op). See `examples/11_hang_classifier.rs`.

### ISR bridge: `IsrBridge` (80 B SPSC ring, no `unsafe`)

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

Admission is capped; the breaker fails fast instead of queueing.

Wrap long-lived services, drivers, and background workers in a `SnubberNode`. The node is a bounded bulkhead with a flight recorder: it counts every path, remembers the last fault with caller file/line coordinates, and stops hammering dead hardware.

```rust
use snubber::{EnterError, SnubberNode};

let node = SnubberNode::builder("payment_gateway")
    .max_in_flight(16)      // at most 16 concurrent executions (17 under EvictOldest)
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
- `SnubberNode::server(name)` — wide bulkhead plus windowed-batch breaker for backend daemons.

### Selective Pass-Through (Canary Law, Tests-Only)

`.passthrough(true)` seals a node as open doors at `build()` — immutable, no runtime toggle. Transit returns a dummy guard that never takes a slot, never touches the breaker or `EpochGate`, and drops the rollback uninvoked (`let _ = rollback;` frees captures without running the body). Every transit bumps `passthrough_admissions` lock-free (`Relaxed`, own column — never merged into `total_started`/`SnapshotV1`, the 512 B wire stays intact). Build two nodes (naked + guarded) and compare: naked leaks, guarded rescues. See `examples/13_canary_differential.rs` and `tests/passthrough_canary.rs`. Never seal a live production perimeter as passthrough.

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
        let _guard = click_node.probe_erased_auto(..)?;
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
   `click_node.probe_erased_auto()`.
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

> **Scope correction.** The PROBE path (`probe_erased_auto`, formerly
> `enter_erased_auto`) is an admission probe.
> It is not a lease on the slot for the worker's lifetime: the probe is
> committed at admission, so the
> slot is free again while the worker keeps running. `max_in_flight(1)`
> on the probe path therefore does NOT serialize two live erased workers
> — it only bounds how many are being *admitted* at once. When the
> ceiling MUST bound live transactions, use the HOLDING path
> (`enter_erased_holding_auto`): the slot stays booked until `commit()`,
> `abort()` or `Drop`, a second admission past the ceiling is refused
> with `BulkheadFull`, and every settle releases exactly one slot. For a
> lifetime slot without the heap box, keep the typed `TrackedGuard` alive
> for the whole loop and commit in the epilogue.

```rust
// Main thread just spawns, holding NO snubber slots:
std::thread::spawn(move || {
    // The worker woke up and takes the probe admission for its own telemetry:
    let guard = match click_node.probe_erased_auto(|| emergency_release_all_hardware()) {
        Ok(g) => g,
        Err(_) => {
            // Bulkhead full, breaker open, rate limited, memory pressure —
            // admission refused. This is NOT proof that another worker is
            // running (see the scope correction above); serialize live
            // workers with your own guard if that is the invariant.
            return;
        }
    };

    // ... the click loop spins for minutes/hours ...

    guard.commit(); // release the admission probe
});
```

### Canonical recipe: `probe_erased_auto` + commit in the epilogue

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
    let guard = node.probe_erased_auto(|| hardware::release_all())?;
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

### Holding recipe: `enter_erased_holding_auto` when the ceiling protects a resource

The probe path proves nothing about how many workers are alive. When
`max_in_flight` is what stands between you and two writers on one serial
port, use the holding entry — the slot is booked for the guard's whole life:

```rust
let guard = node.enter_erased_holding_auto(|| hardware::release_all())?;
// Slot held here. A second admission past max_in_flight is refused with
// BulkheadFull, exactly like `enter`, and the caller is the queue.
guard.commit(); // or guard.abort("stop") / Drop — each releases exactly one slot
```

Everything else matches the probe recipe: one guard per run, settle once,
validate before arming. The difference is only the ceiling.

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
        // The worker armed probe_erased_auto FIRST (lane Normal),
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

Victim set (`PreemptPolicy`, default `BackgroundOnly` — no inversion):
`BackgroundOnly` lets only `Critical` knock, and only when every held
slot is `Background` (`lane_in_flight == [n, 0, 0]`); `Critical` vs
`Normal` is a plain `BulkheadFull`, no flag, `preempted` column clean.
`StrictSenior` widens the victim set to every strictly junior lane
(`Normal` may knock `Background`, `Critical` may knock `Background` +
`Normal`); equal-or-senior occupancy still never preempts:

```rust
use snubber::{PreemptPolicy, SnubberNode};

let node = SnubberNode::builder("clicker")
    .max_in_flight(1)
    .preempt_policy(PreemptPolicy::StrictSenior) // opt-in; default is BackgroundOnly
    .build();
```

The request flag lives until the node fully drains (`in_flight == 0`), not
until the first victim settles: a victim that settles late, or never polls
its conduit, does not silently drop the request.
The node never forces (R5): who never checkpoints `check_directive()`
is its own enemy — the request waits until the worker yields itself.
Diagnostics: `node.preempt_requested()`, `node.preempt_policy()`,
`node.lane_in_flight()`,
`stats().preempted` / `shed_preempted()` (own column, operational shedding, never
trips the breaker, never counted in `admission_total`).

Never stand in the turnstile doorway while calling someone in. If you
walked in yourself — nobody else gets through.

### Execution profile: where the time went (opt-in)

The default `enter*` paths read no profile clock and allocate nothing.
When you need the breakdown, arm a profiled guard — the cost is one `Box`
at admission plus one wall-clock read per timed checkpoint:

```rust
use snubber::{StageKind, SnubberNode};

let node = SnubberNode::builder("export").build();
let mut guard = node.enter_auto_profiled(|| rollback())?;

guard.checkpoint_timed(1, "connect", StageKind::Work);
guard.checkpoint_timed(2, "wait_remote", StageKind::Wait);
guard.checkpoint_timed(3, "write_batch", StageKind::Work);

let (_event, profile) = guard.commit_profiled();
assert_eq!(profile.stage_count, 3);
let slowest = profile.slowest_stage();   // label + wall_us + kind
let pct = profile.cpu_utilization_pct(); // needs a CPU clock
let starved = profile.is_io_starved();   // < 20% CPU while holding a slot
```

* `admit_us` — time inside the admission pipeline.
* `dwell_us` — pre-admission wait (`queue_wait_ms` for sync caller parking).
* `work_wall_us` — guard lifetime from admission return to the commit call.
* `work_cpu_us` — on-CPU time; `0` unless a CPU clock was attached.
* `settle_us` — the commit call itself (does not overlap `work_wall_us`).
* Four stage slots (`PROFILE_STAGES`); further checkpoints set
  `PROFILE_STAGE_OVERFLOW` and merge — they never allocate.

Async admission has the same opt-in wrapper — the profile arms on the
poll that actually admits, so `admit_us` covers the queue wait:

```rust
let guard = node.enter_async_auto_profiled(|| rollback()).await?;
// ... work ...
let (_event, profile) = guard.commit_profiled();
```

Durations are microseconds, saturating at `u32::MAX` (`PROFILE_SATURATED`).
`enter_profiled_with_clock(wall, cpu, rollback)` injects `fn() -> u64`
nanosecond clocks, so tests can drive a static `AtomicU64` and get exact
numbers. `profile.write_summary(&mut out)` renders the ASCII breakdown
into any `core::fmt::Write`.

---

## Layer 3: Runtime-Agnostic Async (Tokio, Axum, gRPC)

One parked waker per wait; any executor drives it, with no runtime dependency.

`snubber` is completely runtime-agnostic: it requires **zero external dependencies** and uses pure `core::task::Waker` parking. The async engine is `std`-gated (it owns OS threads and an admission queue), so it runs identically in Tokio, async-std, smol, or any other `std` executor; `core`-only firmware uses the pulse, ISR and drain layers instead.

### 1. Zero-Runtime Waker Queue (`enter_async` / `enter_async_auto`)
When `max_in_flight` is reached, `enter_async_auto` parks the caller's `core::task::Waker` at **0% CPU** without running background polling threads or spinning:

```rust
let guard = node.enter_async_auto(|| rollback_transaction()).await?;
do_async_io().await?;
guard.commit();
```

Wake-window semantics: when a guard settles, ONE parked waiter is woken and
its `poll` re-runs the full admission check from scratch — the wake is a
hint, not a reservation. Between the slot freeing and the woken task's
re-poll, a concurrent admission may take the slot; the waiter then resolves
to an honest `Err(QueueFull)` instead of hanging, and the caller retries.
The queue also evicts its OLDEST parked waiter (telemetry parity with the
sync `EvictOldest` path) so a queue never hides a wedged consumer.

### 2. Async Settle Across `.await` (`settle_async`)
`TrackedGuard` holds **NO mutex across `.await` points**. The lock is taken only for the
admission/completion bookkeeping — the same short critical sections behind the measured
`node.enter_auto` / `commit` latencies in the benchmark table (tens of nanoseconds on the
bench machine, unbenchmarked in isolation):

```rust
// Runs the future across .await, commits if Ok, aborts and rolls back if Err:
let response = guard.settle_async(async {
    client.post_payment().await
}).await?;
```

### 3. Pacing Without Freezing Worker Threads (`enter_paced_async`)
Never call blocking sleeps (`std::thread::sleep`) in async code. Inject your runtime's timer:

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
    .build();
```

* **`task_ttl`**: Tasks waiting longer than their lease fail fast with `Err(EnterError::QueueTtlExpired)`. The lease clock starts at the **first park** and is never re-stamped: a spurious re-poll, or a settle-wake that loses the free slot to a newer admission and re-parks, keeps the original stamp. TTL therefore bounds total queue residence, not the time since the last park — a woken-but-scooped waiter only gets the remainder of its lease.
* **`queue_capacity` & `shed_policy`**: At capacity, `RejectNew` returns `Err(EnterError::QueueFull)`, while `EvictOldest` discards the oldest waiter. EvictOldest overbooking is capped at exactly one transient slot (`max + 1`): a caller arriving beyond it is refused with `BulkheadFull` like any other shed, and the +1 is an admission already counted in `total_started`, never a double booking.
* **`queue_depth()` / `queue_capacity()`**: Live waiter-queue gauge (`waiters.len()`) and its ceiling. Synchronous nodes have no waiter queue; their pre-admission wait is reported by `TrackedGuard::queue_wait_ms()`.
* **Sync queue horizon (telemetry-only)**: on a synchronous node `queue_capacity` sets the admission-horizon ceiling. When live admissions (`queued` — an admissions counter, not a buffer) reach it, the node records one `BufferOverflow` telemetry event (`buffer_overflow_total`) — the admission itself still proceeds and no policy action (`TripBreaker`, `RollbackAndAlert`, custom) runs on its behalf. The horizon is a backpressure gauge, not a refusal; only the async `RejectNew` queue-full refusal above is a genuine rejection and books `bulkhead_rejections`. `queue_capacity(0)` is legal and means the gauge fires on every admission while any guard is live — nothing is ever refused by the horizon itself. On the async side `0` is floored to 1: exactly one waiter may park and the next is refused with `QueueFull` (the waker-queue capacity is `queue_capacity.max(1)`), so zero never means "no waiters allowed" there.
* **`payload_budget`**: Rejects oversized inputs before memory allocation (`Err(EnterError::PayloadBudgetExceeded)`).

### 6. Memory Limits (external sampler, zero measurement inside)
The crate never reads RSS itself — `no_std` firmware, `/proc`, Win32 and a
Tokio runtime disagree on how. YOUR sampler passes one reading per second:

```rust
use snubber::{MemoryVerdict, SnubberNode};

let node = SnubberNode::builder("worker")
    .max_in_flight(16)
    .memory_limits(64 * 1024, 96 * 1024) // soft 64 MiB, hard 96 MiB (KiB units)
    .build();

// Sampler thread, 1 Hz (read RSS however your platform does):
let verdict = node.note_memory_kb(rss_kb);
match verdict {
    MemoryVerdict::Ok => {}
    MemoryVerdict::Soft => { /* draining: finish in-flight, then restart */ }
    MemoryVerdict::Hard => { /* supervisor terminates WITHOUT draining */ }
}
```

* **Soft (`used >= soft_kb`)**: new admissions fail fast with
  `Err(EnterError::MemoryPressure)`; in-flight guards keep running and settle
  normally (cooperative drain). A later sample below soft clears the flag.
* **Hard (`used >= hard_kb`)**: same refusal, plus the supervisor's
  `DeathCause::MemoryLimit` path may terminate the process immediately.
* **Refusal is lock-free**: admissions read one `AtomicBool` BEFORE the node
  mutex, so a pressured node never contends with the sampler or a snapshotter.
  `retry_after_ms()` advises 1 s and `classify()` maps it to `Refusal::Backoff`.
* **No limits (`0`, the default)**: a single relaxed load per admission; the
  sampler still records `memory_used_kb` / `memory_peak_kb` for telemetry.
* **Counters**: `memory_rejections` is its own column (never a failure
  streak, never trips the breaker); `stats().memory_pressure` mirrors the flag.
* **`MemoryWatch` / `MemoryVerdict`** are `core`-only and `Copy` (32 B), so
  firmware can reuse the verdict math without the `std` node.
* **`LeakWatch<N>` is the derivative, not the level.** `MemoryWatch` answers
  "over budget yet"; a leak is a working set that rises on every sample of a
  full window without one dip. Feed it the same 1 Hz (or 30 s) sample:

  ```rust
  use snubber::{growth_crosses, LeakWatch};

  // 10 samples = five minutes of evidence at 30 s.
  let mut leak: LeakWatch<10> = LeakWatch::new();
  if let Some(growth_kb) = leak.push(rss_kb) {
      if growth_crosses(growth_kb, 120 * 1024) {
          // strictly rising for the whole window and +120 MiB: name a leak
      }
  }
  ```

  `push` returns `Some(newest - oldest)` only when the ring is FULL and every
  sample beats its predecessor; one sample below its predecessor clears the
  window, because a healthy process is allowed to oscillate. `N` is policy,
  not the detector's; `N = 0` never reports. `growth_crosses` is the trip mark
  (`trip_kb == 0` disables it) and stays caller policy: the crate names the
  growth, your app names the threshold.
* **`MemoryTrend<N>` joins the two, and the level outranks the trend.** One
  ring, one verdict per sample: `Hard` is never softened into `Leaking`,
  `Soft` stays `Soft`, and the ring is fed on every sample anyway, so a
  level episode does not blind the trend.

  ```rust
  use snubber::{MemoryTrend, MemoryTrendVerdict};

  let mut trend: MemoryTrend<8> = MemoryTrend::new(soft_kb, hard_kb, 120 * 1024, 30_000);
  match trend.sample(rss_kb) {
      MemoryTrendVerdict::Hard => shut_down_now(),
      MemoryTrendVerdict::Soft => drain_and_restart(),
      MemoryTrendVerdict::Leaking { growth_rate_kb_sec } => {
          // full, rising 30 s window that crossed the trip mark
          log_leak(growth_rate_kb_sec);
          refuse_new_starts();
      }
      MemoryTrendVerdict::Ok => {}
      _ => {} // MemoryTrendVerdict is #[non_exhaustive]
  }
  ```

  `MemoryTrendVerdict` is its own type, not a fourth `MemoryVerdict`
  variant: `MemoryVerdict` has no `repr`, so a field-carrying variant would
  leave every discriminant unspecified (a major change under the semver
  gate). `level()` returns the `MemoryVerdict` half of any trend verdict —
  a leak is `Ok` there, which is the truth: the process is under both
  limits, it is only going up.

  `trend.classification()` answers the shape question without a verdict:
  `Warming { samples }` (not enough evidence), `Stable { growth_kb }` (full,
  rising, under the mark), `Rising { growth_kb, rate_kb_sec }`, and
  `Sawtooth` — full but not monotonic. That last one is the honest gap in
  the derivative rule: a leak that climbs and drops never trips, and this
  variant makes it nameable instead of invisible. Rates divide by the span
  the caller owns: `growth * 1000 / ((N - 1) * interval_ms)`, `None` when
  `interval_ms == 0` or `N < 2`.
* **`HandleWatch<N>` is that rule for counts, not bytes.** A leaked `HANDLE`,
  file descriptor or socket is kernel state — a few bytes of a table, never a
  resident page — so RSS stays flat while the process inches toward its
  per-process limit. The detector is the same derivative, and the crate still
  makes no OS call: the caller passes a probe.

  ```rust
  use snubber::HandleWatch;

  let mut handles: HandleWatch<10> = HandleWatch::new(256, 30_000);
  // Winsock sockets, opened fds, whatever this platform can read.
  let mut probe = || kernel_handle_count();
  if let Some(shape) = handles.sample(&mut probe) {
      if shape.is_rising() {
          refuse_new_starts();
      }
  }
  ```

  `sample` returns `None` **and records nothing** when the probe returns
  `None`: a platform that cannot read a count at that moment must not look
  like a dip, because a `0` would clear the window and silently disarm the
  detector on exactly the host whose probe failed. `note(count)` is the same
  step for callers that already hold the number (a snapshot, a replay).
  `classification()` / `is_leaking()` / `growth_rate_per_sec()` mirror
  `MemoryTrend`, including the `Sawtooth` gap; `trip == 0` disables the
  verdict but keeps the window.
* **Snapshot v6** carries `memory_used_kb`, `memory_peak_kb`,
  `memory_soft_kb`, `memory_hard_kb`, `memory_rejections`, `memory_pressure`,
  `queue_depth` and `queue_capacity`. Still 512 B (330/508 payload bytes used).
* **Anomalies**: `AnomalyKind::MemoryGrowth` fires on any pressured sample
  (ignores `min_sample`), `AnomalyKind::QueueSaturation` fires when
  `queue_depth / queue_capacity > bottleneck_pct`.
* **Checkpointing under pressure**: `soft` is a level, not a deadline —
  see §4.6c for the 64 B ticket layout, the fixed-buffer streaming recipe
  and the cgroup v2 page-cache trap before you serialize any state.

### 7. Bounded Reaper Channel (`AsyncLease` / `LeaseChannel`)
When guards drop during task completion or cancellation, cleanups can be dispatched to background reaper tasks through bounded channels without blocking the worker thread:
```rust
let (tx, rx) = LeaseChannel::pair(256);
// Non-blocking try_send; channel overflow is recorded as FaultKind::BufferOverflow
```

### 8. Zero-Sleep HTTP/gRPC Checkpoint (`AsyncGate`)
For web frameworks (Axum, Actix, Tonic) where sleeping in request handlers is forbidden:
```rust
use snubber::{AsyncGate, Refusal};

let gate = AsyncGate::new(node)?;
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

### 9. Host runtimes: Tokio, Rayon, and the lock you already hold

`snubber` adds no dependency and replaces no runtime: it bounds the entry
points and gives every unwinding path one deterministic exit, while the
runtime underneath keeps doing its own job.

**Tokio.** A cancelled future is dropped, and the admitted `TrackedGuard`
runs its rollback from `Drop` exactly as it does on `Err` or `panic!`. The
ledger books that drop as `FaultKind::ImplicitDrop` (`Error`, catastrophic
for the triage verdict): cancellation is not reclassified as a clean abort.
When a timeout is normal control flow, decide it inside the guarded future
and return `Err`; when it still unwinds, `checkpoint` names the last
completed stage on the fault record and `state_snapshot` gives the live
view. The rollback body runs on the executor thread: keep it short, and
when it can block, arm `rollback_budget_ms` and poll
`snubber::watchdog::poll_rollback_node` from a supervisor tick.

**Rayon.** Wrap one batch in one `scoped`: the rollback runs after Rayon
has joined the workers, so it never races in-flight items, and a panic
continues to the caller of the parallel operation after the rollback. The
closure cannot undo memory — only the compensating action you pass in
makes the batch transactional.

**The lock you already hold.** `SafeLockExt::safe_lock` is the same lock
with a counted poison recovery; it does not replace `Mutex`. That tick is
best-effort (skipped when the ledger is held), and the recovered value may
be inconsistent — revalidate it at the boundary instead of trusting it.

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

The supervisor owns the child process; the drain classifies its wire frames.

When a process is vaporized instantly (`kill -9`, `Stop-Process -Force`, `std::process::abort()`, SIGKILL, or hung Win32 drivers), **no destructors (`Drop`) execute in that process**. Rescue must come from an out-of-process supervisor.

`snubber` provides a turnkey process supervisor with 44-byte framed pipe intercom:

1. **Generation Clock (`GenerationClock`):** The supervisor starts workers with `gen=N`. If the worker dies, the supervisor increments to `gen=N+1`. Any delayed zombie packets from the dead worker are cut off at the gate.
2. **Framed Pipe Intercom (`ProcessToken`):** The worker emits framed 44-byte packets (`b"SNUB"` sync magic + a 40-byte body: `u64` task, `u32` opcode, 28-byte payload).
3. **PipeDrain:** The supervisor drains the stream continuously in a background reader with sliding-window sync, filtering out arbitrary `println!` noise. The same parser is reachable without a reader thread — `drain_sync` (blocking `Read`) and `feed` + `finish` (caller-pushed chunks) — and the liveness clock is injectable (`ClockFn`).
4. **Emergency Rescue on EOF:** If the worker is killed mid-transaction while holding an armed token, the pipe closes (`EOF`), and the supervisor immediately executes the registered emergency hardware release outside the dead process.

### Single-Line Supervised Wire-Up:
```rust
// Your app owns the opcode vocabulary (u32); the crate never guesses it.
const OP_HARDWARE_RELEASE: u32 = 1;

// Worker side: one line links token arming, stdout pipe emit, and rollback
let guard = node.enter_supervised_erased(
    ProcessToken::simple(0, OP_HARDWARE_RELEASE, tag),
    move || { hardware::release_relay(relay_id); }
)?;
```

### Transport and clock injection (`feed`, `write_to`, `ClockFn`)
The wire format is transport-agnostic; only the default helpers assume stdout
and an OS reader thread. Three drivers share ONE incremental scanner:

| Driver | Signature | Use |
|---|---|---|
| `PipeDrain::spawn` | `spawn<R: Read + Send + 'static>(reader)` | Child stdout / OS pipe, own thread |
| `PipeDrain::drain_sync` | `drain_sync<R: Read>(reader)` | In-memory buffers, fuzz sweeps, tests |
| `PipeDrain::feed` | `feed(&self, bytes: &[u8]) -> usize` | UART/DMA chunks, async runtimes, manual pumps |

`feed` accepts any chunking — a byte at a time, several frames per chunk, a
garbage prefix, a torn tail — and returns the number of complete frames
delivered by that call. Call `finish()` when the stream ends: the drain reaches
`EOF`, classifies the rescue and accepts no further bytes.

```rust
use snubber::PipeDrain;

fn app_tick_ms() -> u64 { 0 } // app-owned monotonic ms (firmware SysTick), never SystemTime
let uart_chunk: [u8; 44] = [0; 44]; // chunk pushed by the UART/DMA task

let drain = PipeDrain::manual_with_clock(app_tick_ms); // ClockFn = fn() -> u64, NOT a number
let frames = drain.feed(&uart_chunk);                  // 0..=N complete frames
let idle = drain.idle_ms();                            // measured on app_tick_ms
drain.finish();                                        // EOF, idempotent
```

The injected `ClockFn` (`fn() -> u64`) stamps every parsed frame and answers
`idle_ms` / `expired`. The default is the process monotonic clock; a fake
counter makes lease tests deterministic and a firmware `SysTick` read removes
the hidden `Instant`.

On the worker side, `ProcessToken::write_to(&mut sink)` and
`StateDigest::write_to(&mut sink)` (both `std`-gated) write the 44-byte
frames to any `std::io::Write` (socket, UART bridge, buffer).
`emit_if_supervised` is only a stdout wrapper over `write_to` plus the
`SNUBBER_SUPERVISED` check. On bare metal there is no `Write`: use
`to_bytes() -> [u8; 44]` and feed your own register/DMA/transport.

### Out-of-Process Hard Termination Harness (10,000 OS Kills):
The architecture has been verified against **10,000 real Windows `TerminateProcess` executions** without a single pipe deadlock or lost token. Every reap is classified: **true ambush** (an armed token was still pending at death), **clean escape** (at least one commit was observed) or **stillborn** (no commit and no armed token, typically a kill during process startup). The three counters sum to `reaped` on every run.

`--armed-hold` makes the worker arm once and park; the supervisor publishes the worker pid only after the first arm frame is parsed. A rescue gate of `-MinAmbushPct 70` therefore proves that armed workers were terminated, not startup processes.

For capacity, scale by lineage rather than by threads: `scripts/stress_multi_lineage.ps1` runs N supervisors, each with its own pid file, pipe and killer. The gates on every storm run: per-lane private-memory growth, handle growth, `final_generation == kills + 1`, the per-lane and aggregate bucket conservation (`ambush + escape + stillborn == reaped`) and the ambush-rate floor.

`scripts/stress_multi_lineage.py` is the Linux/macOS equivalent (Python 3 standard library only). It sends `SIGKILL` instead of `TerminateProcess` and verifies `(starttime, image basename)` before every kill: `/proc/<pid>/stat` field 22 plus `/proc/<pid>/exe` on Linux, `ps -o lstart=,comm=` on macOS. A recycled pid, a stale pid file or a foreign image therefore cannot redirect a kill, supervisors are excluded from the target set, and failed identity reads are never cached. Per-lane RSS growth is gated on both platforms; handle growth is sampled from `/proc/<pid>/fd` on Linux and from `lsof -nP -p <pid> -F f` on macOS, so the handle gate applies on both runners. `.github/workflows/stress.yml` runs a 4 x 250 armed-hold storm (70% ambush gate) and a 4 x 500 free-running storm on `ubuntu-latest` and `macos-latest` for every push and pull request.

Measured storm figures (mode rates, dispatch percentiles, bucket splits, CI-runner rows) live in [BENCHMARKS.md](BENCHMARKS.md#multi-lineage-termination-storm-out-of-process-rescue), quoted from the committed reports in `docs/evidence/`.

```powershell
# 10,000-kill Rust integration target:
cargo test --test supervisor_termination -- --ignored --nocapture

# Single-lineage release gate (raw Win32 killer, armed-hold rescue proof):
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 1000 -Launch -RawKill -Aggressive -ArmedHold

# 4 independent lineages under a parallel death storm:
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold
```

```bash
# Same storm on Linux / macOS (SIGKILL path):
python3 scripts/stress_multi_lineage.py --lines 4 --kills-per-line 2500 --aggressive --armed-hold
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
    match tracker.note_death(Some(code), now_ms()) {
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

// --- child process side (same crate) ---
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

### 4.6b Memory euthanasia (`MemoryPolicy`, Phase 3)
Same external-sample law as §6, supervisor side: the APP's sampler reads the
child's RSS, `MemoryPolicy` only does integer compares and names the verdict.

```rust
use snubber::{DeathCause, MemoryPolicy, Supervisor};

let policy = MemoryPolicy::new(64 * 1024, 96 * 1024); // soft drain, hard kill
// Sampler: pid -> resident KiB (the APP's platform call: /proc, Win32, cgroup).
let sample = |pid: u32| read_rss_kb(pid);
Supervisor::run_supervised_guard_with_memory(
    "--worker", 5_000, policy, sample, on_dead_token,
);
```

* **Soft** — logged once as `[SUPERVISOR:MEMORY_DRAIN]`. Telemetry for the
  operator, NOT a control message: the pipe is one-way (child → out), so the
  supervisor cannot command the drain. The worker is expected to sample its
  own memory (`SnubberNode` soft limit → `EnterError::MemoryPressure`),
  refuse admissions, drain in-flight guards, emit a `StateDigest` (§4.6) and
  exit for a successor generation. The supervisor never kills at soft.
* **Hard** — `child.kill()` WITHOUT draining; the death is stamped
  `DeathCause::MemoryLimit` (`"process exceeded hard memory limit"`, never
  confused with `HangTimeout`).
* **`hard < soft`** clamps up to soft, so a drain phase always exists;
  `0` disables a threshold; both zero keeps the plain lease loop.
* `policy.watch()` exposes `used_kb()` / `peak_kb()` / limits for the
  postmortem; `policy.death_cause()` is `Some(MemoryLimit)` only at hard.
* The whole chain — soft → refuse → digest → generation bump → successor
  admits — runs in `examples/14_memory_recycle.rs`; the real two-process
  loop (supervisor + worker, drain mode vs `--no-drain`) runs in
  `examples/15_supervised_memory.rs`; the bounded-heap checkpoint itself
  (64 KiB streaming vs `--naive` clone) runs in
  `examples/16_streaming_checkpoint.rs`.

### 4.6c Checkpoint physics: 64 B is a ticket, not storage

`StateDigest` is exactly 64 bytes (`STATE_LEN`, compile-time gate
`STATE_LEN == 64`). The crate treats the bytes as opaque: it computes one
CRC32 over all 64 bytes (`StateDigest::crc`), splits them into three
`OP_STATE` frames (24 + 24 + 16 B), reassembles them in `PipeDrain` and
silently drops any torn or mismatched set. The field layout below is the
recommended worker-to-successor protocol; the crate does not parse it.

> **Level Budget, Not Time Budget**
> `hard_kb - soft_kb` is not a reserve of seconds. It is the allocator's
> headroom. A process that normally holds 64 MiB with `hard_kb = 96 MiB`
> has 32 MiB of one-shot headroom; a single allocation above that is
> killed on the next sampler tick (10 ms), before it can flush state.

> **The Ticket Law**
> `StateDigest` stores nothing. It certifies that a file was already
> written, synced and sealed by `rename`. A process killed mid-write has
> no sealed file, so no ticket is emitted, and the successor must roll
> back to the previous sealed checkpoint.

#### Fixed layout (64 B, explicit little-endian)

| Offset | Size | Field | Encoding |
|---|---|---|---|
| `[0..4]` | 4 | Magic | `b"SNBK"` (Snubber ChecKpoint) |
| `[4..6]` | 2 | Version | `u16`, `0x0001` |
| `[6..8]` | 2 | Flags | `u16`: bit 0 `COMPRESSED`, bit 1 `ENCRYPTED`, bit 2 `DIRTY_RESUME` |
| `[8..16]` | 8 | Generation | `u64` from `GenerationClock` / `SNUBBER_GEN` |
| `[16..24]` | 8 | Sequence | `u64`, last committed batch/transaction |
| `[24..32]` | 8 | Total bytes | `u64`, exact size of the sealed file |
| `[32..36]` | 4 | CRC32 | `u32` over the file body as written |
| `[36..40]` | 4 | Reserved | `u32`, zero |
| `[40..64]` | 24 | Payload | app slot: file UUID, offset, hash — or zeros |

Every field is written with `copy_from_slice(&value.to_le_bytes())`;
reserved and unused payload bytes stay `0x00`. No padding, no
`transmute`, no uninitialized memory reaches the pipe.

```rust
fn seal(gen: u64, seq: u64, total: u64, crc: u32, file_id: &[u8; 16]) -> StateDigest {
    let mut d = [0u8; STATE_LEN];
    d[0..4].copy_from_slice(b"SNBK");
    d[4..6].copy_from_slice(&1u16.to_le_bytes());
    d[6..8].copy_from_slice(&0u16.to_le_bytes()); // flags
    d[8..16].copy_from_slice(&gen.to_le_bytes());
    d[16..24].copy_from_slice(&seq.to_le_bytes());
    d[24..32].copy_from_slice(&total.to_le_bytes());
    d[32..36].copy_from_slice(&crc.to_le_bytes());
    d[36..40].copy_from_slice(&0u32.to_le_bytes()); // reserved = 0
    d[40..56].copy_from_slice(file_id);
    // [56..64] stays zero
    StateDigest::new(d)
}
```

Successor validation is all-or-nothing: a wrong magic, unknown version,
size mismatch or file CRC mismatch makes the ticket dead. Note the split:
the 3-frame pipe path (`emit_if_supervised` -> `PipeDrain`) verifies the
digest CRC and drops torn sets, but `take_stdin_digest` is a plain 64 B
`read_exact` (a short read yields `None`) — it does not re-verify the CRC.
Treat magic/version/body CRC as the successor's own gate.

```rust
fn restore(digest: &StateDigest, path: &Path) -> std::io::Result<Vec<u8>> {
    let d = &digest.bytes;
    if &d[0..4] != b"SNBK" || u16::from_le_bytes([d[4], d[5]]) != 1 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "dead ticket"));
    }
    let total = u64::from_le_bytes(d[24..32].try_into().unwrap());
    let want = u32::from_le_bytes(d[32..36].try_into().unwrap());
    let body = std::fs::read(path)?;
    if body.len() as u64 != total || crc32(0, &body) != want {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "torn checkpoint"));
    }
    Ok(body)
}
```

#### Why a naive serialization dies (50 MB state)

`serde_json::to_vec(&state)` — or a plain `state.clone()` — allocates a
second copy of the state. A worker at 64 MiB resident that crosses soft
and then builds a 50 MiB copy reaches 114 MiB, above `hard_kb = 96 MiB`.
The supervisor's 10 ms sampler sees the crossing and calls `child.kill()`
before the first `write_all` reaches the disk. Writing 50 MB at 500 MB/s
takes ~100 ms plus `sync_all`; the level crossing is not a countdown.

#### Streaming recipe (bounded heap)

```rust
const BUF: usize = 64 * 1024;

fn write_checkpoint(path: &Path, state: &[u8]) -> std::io::Result<u32> {
    let tmp = path.with_extension("tmp");
    use std::io::Write;
    let mut file = std::fs::File::create(&tmp)?;
    let mut buf = [0u8; BUF]; // stack, reused for every chunk
    let mut crc = 0u32;
    for chunk in state.chunks(BUF) {
        buf[..chunk.len()].copy_from_slice(chunk);
        crc = crc32(crc, &buf[..chunk.len()]);
        file.write_all(&buf[..chunk.len()])?;
    }
    file.sync_all()?;          // durability before the seal
    drop(file);
    std::fs::rename(&tmp, path)?; // atomic on POSIX and NTFS
    Ok(crc)
}
```

Peak heap delta is the 64 KiB stack buffer, independent of state size.
Emit the ticket only after `rename` returns:

```rust
let crc = write_checkpoint(&path, state)?;
seal(gen, seq, state.len() as u64, crc, &file_id).emit_if_supervised();
std::process::exit(0);
```

#### cgroup v2: page cache counts against the limit

* Linux container limits are enforced on `memory.current`.
* It includes anonymous memory **and** the page cache of written files.
* A 50 MB `write()` leaves 50 MB of cached pages in the kernel. The heap
  did not grow, but the cgroup counter did.
* With a 150 MB container, a worker at 110 MB that writes a 50 MB
  checkpoint reaches ~160 MB: the kernel OOM-killer terminates the process
  inside `write_all`, before any supervisor logic runs.

Close it in the app: after `sync_all()`, drop the pages with
`posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED)` on Linux. Dirty pages must
be synced first — `fadvise` cannot evict them. On Windows,
`GetProcessMemoryInfo` reports the process Working Set and does not
account file cache; `sync_all` (`FlushFileBuffers`) is still required for
durability. A cgroup-aware sampler should read the `anon` field of
`memory.stat` for heap pressure, or budget the checkpoint size into
`hard_kb`.

#### Ownership split

| Guarantee | Owner |
|---|---|
| 64 B ticket, CRC over all 64 B, 3-frame transport, torn frames dropped | crate |
| Threshold verdicts (`MemoryVerdict`), hard-kill decision, 10 ms sampling | crate |
| Serialization, 64 KiB buffer, `.tmp` + `sync_all` + `rename`, `fadvise`, file CRC | app |
| `soft`/`hard` values such that `hard - soft` covers the checkpoint peak | app |

If the process is killed mid-stream, no complete digest reaches the drain
(`PipeDrain::last_state` keeps the previous one) and the `.tmp` file is
never renamed. The successor reads stdin via `spawn_with_digest` /
`take_stdin_digest`, revalidates magic, version, size and CRC, and on any
mismatch rolls back to the previous sealed checkpoint.

**Reference:** `examples/16_streaming_checkpoint.rs` runs both paths
end-to-end — streaming seals 16 MiB and the successor reports
`restored_bytes=16777216`; `--naive` clones the state, crosses `hard_kb`
and is killed mid-`write_all`, leaving `state.tmp` and no ticket.
`tests/checkpoint_ticket.rs` covers the dead-ticket matrix (magic,
version, size, CRC, torn write), `tests/checkpoint_budget.rs` counts heap
bytes through a `#[global_allocator]` (streaming peak ≤ 128 KiB), and
`tests/checkpoint_loop.rs` drives two real supervisor/worker/successor
process chains.

### 4.6d Worker pool (`WorkerPool`): N lineages, one pipe per worker

One `Supervisor` is one lineage. Its `PipeDrain` keeps ONE armed slot and
ONE `OP_STATE` scratch slot, and `take_armed()` returns a token without a
lineage id — two workers behind one supervisor would merge rescue
accounting, and one silent child would stall every waiter behind
`child.wait()`. The pool keeps the wire format and the drain semantics
intact and scales the COUNT:

| Part | Single lineage | Pool |
|---|---|---|
| Drain | one stdout | one `PipeDrain` per lane |
| Generation | one `GenerationClock` | one per lane, bumped once per reported death |
| Restart | one `RestartTracker` | one per lane (own backoff and storm window) |
| Memory | one `MemoryPolicy` | one per lane, reset on `attach` |
| Wait | blocking `child.wait()` | one app thread per lane |

```rust
use snubber::pool::{namespace_task, PoolConfig, WorkerPool};
use snubber::{Backoff, PipeDrain, RestartDecision, RestartPolicy, RestartWindow};

let config = PoolConfig::new(4).with_restart(
    RestartPolicy::Always,
    Backoff::none(),
    RestartWindow::unlimited(),
);
let pool = WorkerPool::new(config)?;
let lane = pool.lane(0).unwrap().clone();                 // one Arc per app thread
let generation = lane.attach(pid, PipeDrain::spawn(stdout))?;
// Worker emits ProcessToken { task: namespace_task(0, seq), .. }.
let reap = lane.note_death(Some(exit_code), now_ms)?;
match reap.decision {
    RestartDecision::Restart { delay_ms, .. } => app_sleep(delay_ms),
    RestartDecision::Stop | RestartDecision::GiveUp { .. } => app_parks(lane.index()),
}
```

`note_death` waits up to `eof_grace_ms` (default 250 ms) for EOF and then
classifies from THAT lane's drain only: **ambush** (un-settled arm, token
returned for the app's rollback), **escape** (commit seen), **stillborn**
(neither). A pipe that never reaches EOF inside the grace window is a
fourth honest bucket — `ReapClass::Deadlocked`, no token claimed — and
`PoolReport::healthy()` is false until `deadlocks == 0`. Conservation
holds per lane and in the totals:
`ambush + escape + stillborn + deadlocks == reaps`.

App ownership is unchanged: the app spawns, kills, sleeps and owns pid
files; the crate owns the classification, the per-lane restart math and
the aggregated `PoolReport` (ASCII card + totals). Task ids can be
namespaced (`task = lane << 48 | local`) so a rescued token names its
lineage: `namespace_task`, `task_lane`, `task_local`, `MAX_LANES = 2^16`.

A thread blocked in `child.wait()` sees only its own lane, so one app-side
monitor calls `pool.expired_lanes(lease_ms) -> Vec<(lane, idle_ms)>` on a
tick and kills the lanes it names itself. Only lanes that already parsed a
frame can expire: a child that hung before its first heartbeat is the
app's spawn timeout, not a lease breach. On Windows this is what covers a
worker that answers once and then never exits.

Measured costs (release, Criterion `layer4_worker_pool`): every lane
locks only its own state, so per-reap figures scale with lanes, not with
the pool (per-operation numbers: [BENCHMARKS.md](../BENCHMARKS.md)). The
storm gates on this machine (debug; 2026-10-26 committed artifacts in
`docs/evidence/`): 4 lanes × 8 armed kills = 32/32
ambush, `wall_ms=55`; the mixed storm produces an equal four-way split (A/E/S/D =
8/8/8/8, 8 rescued) with `deadlock=true` and `wall_ms=474` — the orphan mode
keeps its pipe open past the 150 ms grace, so the deadlock bucket is
proven, not simulated. The lease storm kills 24 workers that beat once
and then hang (4 lanes × 6, 25 ms lease): 24/24 stillborn, `lease_kills=24`,
0 deadlocks, 0 conservation breaks over 99,628 live `report()` samples
from the observer thread, `[7,7,7,7]` generations, `wall_ms=194`.
`wall_ms` and the observer sample count are wall-clock observations and
move with load; the bucket counters and generations are deterministic.

**Reference:** `examples/19_worker_pool.rs` (supervisor and worker modes,
`--mixed` four-class storm, `--lease-ms` euthanasia storm, JSON report),
`tests/supervisor_pool.rs` (21 deterministic tests: lane isolation,
per-lane generation, conservation, per-lane backoff/window, attach/reap
errors, EOF-grace and zero-grace deadlock, live-pipe lease expiry, the
`expired_lanes` sweep naming only the silent lane, Stop/GiveUp delay
contract, hard-limit clamp, task namespace, per-lane memory, state
handoff, 4 lanes × 25 threaded cycles), `tests/supervisor_pool_stress.rs`
(`#[ignore]`: 4 lanes × 8 hard kills, gates `reaped`, `ambush`,
`deadlock`, `final_generation`), `tests/supervisor_pool_stress_mixed.rs`
(`#[ignore]`: armed/clean/silent/orphan cycle, gates the four buckets and
conservation), `tests/supervisor_pool_stress_lease.rs` (`#[ignore]`:
heartbeat-then-hang children, gates `lease_kills`, `conservation_breaks`
and generations). Each storm also gates app-side token receipt
(`tokens_seen`) and lineage (`token_lane_mismatch`), so the pool's
`rescued` counter is checked against a witness rather than against
`ambush`, which it shares an increment with. The three pool storms and
`stress_breaker_churn` run in the `release-ignored` CI job of
`.github/workflows/ci.yml` on every push and pull request; the heavier supervisor and multi-lineage harnesses are
scheduled or dispatch-only.

### 4.7 Orphan contract (Phase 6: no zombies)
```rust
use snubber::{NoOrphanGuard, OrphanGuard};
let guard = NoOrphanGuard; // honest no-op; real Win32 Job Objects implemented in app
assert!(!guard.arm(child_pid));
```

### 4.7b Liveness witness (`Sentinel`): naming death without a syscall

`Sentinel` answers one question for a process the crate does not own:
*is it still there, and is anything still armed?* The OS call
(`OpenProcess`, `pidfd_open`, a Job Object handle) is application code
behind `ProcessWitness`; the crate keeps the arithmetic in `core` — no
clock, no allocation, no system call.

```rust
use snubber::{classify_liveness, NoWitness, Sentinel, SentinelConfig, SentinelSample};

let cfg = SentinelConfig::new(250); // idle grace in ms; 0 disables `Silent`
let sample = SentinelSample {
    idle_ms: 40,
    armed: true,
    witness_alive: NoWitness.alive(), // `None` = cannot observe
};
assert_eq!(classify_liveness(cfg, sample), snubber::SentinelVerdict::Alive);

let mut sentinel = Sentinel::new(cfg);
let verdict = sentinel.observe(sample);
assert_eq!(sentinel.last(), verdict);
```

| Witness says | Token silence | Verdict |
|---|---|---|
| `Some(false)` | any | `Dead { armed }` — death outranks silence |
| `None` | `idle_ms > grace` | `Silent { idle_ms }` — never `Dead` |
| `Some(true)` / `None` | `idle_ms <= grace` | `Alive` |

The comparison is strict: `idle_ms == grace` is still `Alive`. The crate
never kills and never knows a pid — a `Dead` verdict is returned to the
owner, which runs `kill`/`TokenRollback`. `tests/sentinel.rs` pins the
rules; the unit tests run in the `core`-only lane.

The pipe side is one call each: `PipeDrain::sentinel_sample()` fills the
sample from drain state (`witness_alive: None` — the drain sees traffic,
not the process table), and `await_post_mortem` is the EOF-grace rule
`run_loop` itself uses:

```rust
use snubber::{await_post_mortem, PipeDrain, Sentinel, SentinelConfig};

// After the child was reaped: wait out the grace, then take what is armed.
let token = await_post_mortem(&drain, 250); // `None` = clean exit

// The liveness side of the same pipe.
let mut sentinel = Sentinel::new(SentinelConfig::new(250));
let verdict = sentinel.observe(drain.sentinel_sample());
```

Waiting is not optional: a bare `take_armed()` races the reader thread and
reports CLEAN while the child's last frame is still in flight — the
hardware stays armed after the crash. `tests/sentinel_pipe.rs` fails that
injection.

### 4.8 Impedance bridge (Phase 4): conflation, envelope, harness

A 10 000 msg/s producer feeding a 60 FPS consumer is a 166:1 mismatch. There
are three honest answers; pick by what a stale value is worth:

| Topology | Queue length | Stale data | API |
|---|---|---|---|
| Raw stream | unbounded | delivered late, forever | app-owned `mpsc` |
| **Conflation** | **0 or 1** | dropped, counted | `ConflateSender` / `ConflateReceiver` |
| Pacing | bounded | refused with an advised wait | `gateway` HWM 75% / LWM 50% |

**Conflation.** One slot, latest wins. A publish into an occupied slot
replaces the unsampled value and increments `conflated`:

```rust
use snubber::ConflateSender;
let (tx, rx) = ConflateSender::<u64>::pair();
assert!(!tx.publish(1));   // empty slot: plain publish
assert!(tx.publish(2));    // occupied: coalesced
assert_eq!(rx.take_latest(), Some(2));
assert_eq!(rx.take_latest(), None);
assert_eq!(rx.conflated(), 1);
```

Counter law: `published == taken + conflated + occupied`, where `occupied`
is 0 or 1. `publish` returns the replacement flag, so a producer can count
coalescing without a second read. The channel is the `TaskConduit` draft
table (`TaskConduit::publish_draft` returns the same flag,
`conflated_drafts()` mirrors the counter on both ends), so it carries no
`Clone` bound on the payload and never blocks on the consumer. The consumer
samples at its own cadence; `None` means "no publish since the last take",
never "lagging".

**Execution envelope.** A 16-byte `Copy` corridor declared on the node,
judged once per settlement:

```rust
use snubber::{ExecutionEnvelope, SnubberNode};
let env = ExecutionEnvelope::new(100, 1_000, 64, 8_192); // µs, bytes
let node = SnubberNode::builder("quotes").execution_envelope(env).build();

let guard = node.enter_auto_profiled(|| {}).expect("admit");
// ... work ...
let (_event, profile, verdict) = guard.commit_enveloped(payload_bytes);
```

* `0` disables an individual bound; all four zero disables judgement.
* `max < min` raises the ceiling to the floor (the floor is the stronger
  promise). Bounds are inclusive.
* The wall half is judged only for profiled entries
  (`enter_profiled*` / `enter_auto_profiled`) from
  `ExecutionProfile::work_wall_us`; a plain guard has no clock evidence and
  gets `TimeVerdict::Inside`. The payload half is exact for any guard.
* Floor misses tick `SnubberStats::envelope_underflows`, ceiling misses tick
  `envelope_overflows`. Neither feeds the breaker, neither changes
  `SnubberEvent`, neither enters `SnapshotV1` (the 512 B frame stays v6).
* An underflow is the silent-skip signal: work returned faster or smaller
  than promised, so the cache was stale or the loop never ran.
* `pad_to_floor(wall_us)` / `pad_payload_to_floor(bytes)` raise a value to
  the declared floor and leave anything above it untouched. It exists for the
  caller that must not finish faster than its floor — a pacer, a coalescing
  window — which pads its own WAIT with the result. Padding the reported
  duration of work that did not happen is the very signal the floor is there
  to catch, so pad the wait, never the story; the ceiling is unaffected and
  an overrun still reads `Over`.

**Harness.** Measure first, promise second:

```rust
use snubber::ProfileHarness;
let report = ProfileHarness::new(64, 4, 3)
    .payload_range(4_000, 12_000)
    .run(0x5EED, |bytes| { /* workload */ let _ = bytes; });
println!("{}", report.summary());
let envelope = report.envelope(50); // floor -50%, ceiling +50%
```

The matrix is **min / median / max**, **jitter** (`max - min`) and
**bursts** — non-overlapping runs of `burst_len` consecutive samples
strictly above the median. `FastPrng` (xorshift32) makes the payload
sequence reproducible from the seed; `run_with_clock` accepts an injected
nanosecond clock, so tests assert exact microseconds.
`run_boundary_harness(samples, seed, work)` is the one-shot default plan
(64 samples, 4 warm-up, burst 3, payload 0).

**Reference:** `examples/17_impedance_bridge.rs` runs one producer through
the raw stream, the conflation slot and the paced gateway side by side;
`examples/18_impedance_calibration.rs` measures a workload, derives an
envelope and judges real settlements, including a deliberate no-op
underflow. Tests: `tests/conflation.rs` (1M publishers, exact coalescing,
payload drop accounting, non-`Clone` payload), `tests/execution_envelope.rs`
(scripted clocks: inside / under / over / zero-disabled / clamp),
`tests/harness.rs` (scripted clock matrix and payload range).

---

## Layer 5: Diagnostics & Health (Catastrophic Faults vs Capacity Shedding + `app.snub`)

One 512-byte snapshot answers whether the system is healthy.

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

### Snapshot diff: what changed since the last sample

```rust
let before = node.snapshot_v1();
// ... 10 s later ...
let after = node.snapshot_v1();

let diff = before.diff(&after); // every field saturating: restarts yield zeros
assert!(!diff.is_empty());
let mut text = String::new();
diff.write_summary(&mut text)?;
// diff: admitted +120 committed +118 aborted +2 dropped +0
//       bulkhead +3 evicted +0 preempted +0 rate +0 stale +0
//       breaker CLOSED->CLOSED trips +0 timeouts +0 pulse +0 lag +0ms
//       memory 4096->6144kb peak +8192kb reject +0 pressure ok->PRESSURE queue 0->2
```

`SnapshotDiff::catastrophic_faults()` gives the panic + timeout +
process-died + implicit-drop delta; `breaker_state_from/to` show a
transition inside the window; memory/queue gauges are reported as
`from/to` pairs (`memory_peak_kb` and `memory_rejections` are monotonic,
so they are deltas). Pure `Copy` math — no wire change.

### Triage card: one ASCII verdict per node

```rust
use snubber::write_triage;

let mut card = String::new();
write_triage(&mut card, "click_worker", &snap)?;
// [SNUBBER TRIAGE] node="click_worker" verdict=OK
//   breaker  CLOSED
//   admits   124000 (committed 123858 / 99%)
//   faults   drops=0 panics=0 timeouts=0 died=0
//   shed     bulk=0 evict=0 rate=142 stale=0 preempt=0
//   pulse    stalls=0 lag=0ms
//   memory   used=4096kb peak=8192kb soft=65536kb hard=98304kb pressure=ok rejects=0
//   queue    depth=0 cap=64 (0%)
```

`verdict=CRIT` on any catastrophic fault, `WARN` on shedding, a missed
pulse, a non-closed breaker, memory pressure, or a full waiter queue,
`OK` otherwise. Pure `fmt::Write`, no allocation — works on firmware and
in a stack buffer.

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
CPS in a hot loop (the `last_fault` clone shows up in the
profiler at 160 CPS).

### Ordered teardown (`RankedNode` / `RankedSet`)

`NodeSet` aggregates; it does not order. A stop loop that walks
registration order loses the last frames whenever a sink is stopped before
the engine that writes to it — an ordering fact that no test can see,
because both orders compile. The rank lives in the type:

```rust
use snubber::{RankedNode, RankedSet};

let mut stop: RankedSet<'_, 4> = RankedSet::new();
stop.insert(RankedNode::<1>::new(&log_sink))?;   // kept alive longest
stop.insert(RankedNode::<9>::new(&engine))?;     // stopped first
for entry in stop.iter() {
    shutdown(entry.node());                       // highest rank first
}
```

* **Higher `RANK` is stopped first**, ties keep insertion order. The set
  sorts at insert (one backward shift inside a fixed array), so the loop
  reads front to back — no `Vec`, no sort at read time, no allocation.
* **The rank is a type parameter, not a field.** `RankedNode::<3>::new(..)`
  says the same thing at every call site; a `rank: u8` argument is a number
  any caller can get wrong. The set stores it as data only because one set
  holds entries of different `RANK` types (`RankedEntry::rank`).
* **A full set refuses with `RankedSetError::Full`** and keeps its order; it
  never evicts, because the member that vanished is the node the loop
  forgets to stop. Size `CAP` for the topology, not for the traffic.
* `iter()`, `get(index)`, `len()`, `capacity()`, `clear()`; `RankedNode` and
  `RankedEntry` forward `name()`, `snapshot_v1()` and `stats()`.

### Flight tape (`FlightTape<N>`)

The node's flight recorder is a `VecDeque` behind `std`, and the moment it
is most wanted is the moment the process is out of memory — an allocator
call on the way to a crash is the first thing to fail. `FlightTape<N>` is
the same idea in a fixed array, so it also works on firmware:

```rust
use snubber::FlightTape;

let mut tape: FlightTape<16> = FlightTape::new();
tape.record_code(epoch, task_id, code);      // one event, no allocation
// …the process dies here…
for entry in tape.iter() {                   // oldest → newest
    log(&entry.epoch, &entry.task_id, &entry.code);
}
```

* **It records `(epoch, task_id, code)` and nothing else** — 24 B per cell,
  `Copy`. No file, no line, no text: those are `&'static str` in
  `FlightSlot` and belong to the `std` recorder. `code` is your vocabulary
  (a `FaultKind::index()`, a transition number, an `OP_*` byte); the crate
  never interprets it.
* **A full tape overwrites its oldest cell**, so it always holds the LAST
  `N` events in order. `N = 0` records nothing instead of panicking — a tape
  sized from a config value must survive a zero.
* `last_with_code(code)` walks newest first, which is the question an error
  handler actually asks: "what was the last thing this task did".
  `newest()`, `oldest()`, `at(i)`, `len()`, `is_full()`, `capacity()`,
  `clear()` round it out.

---

## Extensibility: hooks, names, gates

### Public verdict enums are `#[non_exhaustive]`

`EnterError`, `FaultKind`, `AnomalyKind`, `ReapClass`, `RestartDecision`,
`Directive`, `Refusal`, `GateError`, `MemoryVerdict`, `DeathCause`,
`GatewaySendError`/`GatewayRecvError`, `PoolError`, `IsrPushError`, `RankedSetError`,
`StallKind`, `TimeVerdict`, `PayloadVerdict`, `SnapshotError`,
`BreakerState`, `SnubberError`, `StageKind` and `TimeoutCheck` carry
`#[non_exhaustive]`. Two consequences:

* Your matches against them must keep a `_` arm. A new verdict compiles
  against your code and hits your arm explicitly — it is never silently
  reinterpreted.
* Inside the crate the matches stay exhaustive on purpose: the
  attribute only binds downstream crates, and a wildcard there would be
  dead code.

### Capture-capable backpressure hooks

`.friction_fn(f)` accepts any `Fn(u32, u32) -> u64 + Send + Sync +
'static` — a plain `fn` still compiles unchanged, a capturing closure now
compiles too. The handler is stored behind `Arc<dyn Fn>` and invoked
OUTSIDE the node lock (the lock is dropped before the call, so a handler
may re-enter the node). `.friction_fn_ptr(f)` is the explicit
fn-pointer wrapper kept for code that names `FrictionFn` in signatures.

`.pressure_fn(f)` accepts any `Fn(Backpressure) + Send + Sync + 'static`
and puts the node in custom-reaction mode: saturation refuses the caller
without parking and hands the readings to your closure. It is the
closure-friendly replacement for `BackpressurePolicy::Custom(fn)`, which
stays as the legacy variant. Measured cost of the dynamic dispatch and
the full `friction_wait_ms` path (identical with either storage on the
reference machine): [BENCHMARKS.md](../BENCHMARKS.md).

### Adaptive ceiling (`AimdLimit`) and jittered retries (`jitter_ms`)

A fixed `max_in_flight` is a guess made at build time; AIMD replaces the
guess with the largest limit the downstream actually sustained:

```rust
use snubber::AimdLimit;

let mut ceiling = AimdLimit::new(4, 1, 16, 1, 50);
// once per window, not once per admission
let limit = if window_was_clean {
    ceiling.on_success()      // +step, clamped to max
} else {
    ceiling.on_pressure()     // keep 50 %, never below min
};
```

* **One outcome per window.** An additive step per admission grows the
  ceiling without bound between two samples — that is not AIMD, it is noise
  with a counter.
* **`keep_percent` is clamped to `1..=99`**: `0` collapses to `min` and
  `100` never shrinks, and neither is a decrease. `min` is a floor for a
  reason — a ceiling of `0` refuses everything forever.
* **`Backpressure::jittered(spread_percent, salt)`** moves only
  `retry_after_ms`; depth and kind stay the diagnosis. The offset comes from
  the SplitMix64 finalizer over your `salt`, so the band is symmetric around
  the node's advice and its mean stays put:

  ```rust
  use snubber::{jitter_ms, Backpressure, FaultKind};

  let task_id = 42u64;                       // per waiter: id, attempt, thread index
  let advice = Backpressure::new(1_000, 7, FaultKind::BulkheadFull);
  let mine = advice.jittered(20, task_id);   // 800..=1200 ms, same salt → same value
  assert_eq!(jitter_ms(1_000, 20, task_id), mine.retry_after_ms);
  ```

  No RNG and no clock: the crate owns no entropy source, so the caller
  supplies variation and gets determinism back (that is what makes the band
  testable). Pass a salt that differs per waiter — `salt == 0` hashes to `0`
  and always lands on the low edge, i.e. no jitter at all.

### Runtime node names

`SnubberNode::builder` accepts `impl Into<NodeName>`:

```rust,no_run
use snubber::{SnubberNode, NodeName};

let a = SnubberNode::builder("static/label").build();          // Static, no allocation
let b = SnubberNode::builder(format!("shard-{}", 7)).build();  // Owned, Arc<str>
let c = SnubberNode::builder(NodeName::owned("worker-3".to_string())).build();
```

`node.name()` returns `NodeName` (clone: a fat-pointer copy for
`Static`, one `Arc` increment for `Owned`). `FaultRecord::node` carries
the same label. The `Copy` telemetry frames (`SnapshotV1`, `MetricSample`,
`Anomaly`, wire headers) stay `&'static str`: metric rows and anomaly
verdicts for an owned-name node export the marker `<owned>` — read
`FaultRecord::node` or `node.name()` for the exact label.

### Mapped invariant gate

```rust
use snubber::snub_check_map;

#[derive(Debug, PartialEq)]
enum Gate { Amount }

impl From<&'static str> for Gate {
    fn from(_: &'static str) -> Self { Gate::Amount }
}

fn check(amount: i64) -> Result<(), Gate> {
    snub_check_map!(amount > 0, "amount must be positive");
    Ok(())
}
assert_eq!(check(-1), Err(Gate::Amount));
```

`snub_check!(cond, reason)` is unchanged (`&'static str` early return);
`snub_check_map!(cond, err)` routes the reason through `From` into the
function's own error type.

### Explicit-argv supervision

`Supervisor::run_supervised_guard`, `..._with_lease` and
`..._with_memory` have `_argv` twins that take the full argument vector
(index 0 is the program name, same shape as `std::env::args()`). Tests,
embedded launchers and shells that parsed their own command line use
them instead of inheriting the process argv. Without `--supervisor`, or
when `worker_flag` is present, they return `false` immediately — a
cheap unit-test hook.

### Format ceilings are a contract

Published constants, compile-asserted in the source, not tunables:
3 priority lanes (`Priority::COUNT`), 4 inline profile stages
(`PROFILE_STAGES`), 16 ISR ring slots (`ISR_RING_CAP`), 28 B token
payload / 40 B body (`TOKEN_PAYLOAD_LEN` / `TOKEN_BODY_LEN`), 512 B
snapshot frame (`SNAPSHOT_LEN`), 250 ms default pool EOF grace
(`DEFAULT_EOF_GRACE_MS`). Changing any of them is a wire-format version
bump, not a patch.

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

`with_cpu_sampler(sampler: fn() -> u64)` accepts a raw monotonic tick counter. The crate calculates differential math in pure `core`. When the backend must read a per-thread handle or a captured worker counter, use `with_cpu_sampler_shared(Arc<dyn Fn() -> u64 + Send + Sync + UnwindSafe + RefUnwindSafe>)` — same arming law, `std`-only, and the auto-trait bounds keep `TrackedGuard` `UnwindSafe`/`RefUnwindSafe` as in 0.6.x:

| Target Platform | Syscall / Instruction | App-Side Sampler Implementation |
|---|---|---|
| **Windows** | `GetThreadTimes` | Returns `(KernelTime + UserTime) / 10_000` (ms). |
| **Linux (glibc / musl)** | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` | Returns `ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000`. |
| **ARM Cortex-M (no_std)** | `DWT->CYCCNT` | Read cycle counter directly: `DWT::cycle_count() / (CPU_FREQ_HZ / 1000)`. |

---

## API & Cost Quick Reference

Measured latencies live in [BENCHMARKS.md](../BENCHMARKS.md) — the single
numeric home, pinned by
`tests/doc_sync.rs::test_bench_numbers_match_evidence_artifact`; this
table is the signature, layer and footprint dictionary and quotes none
of them. Sub-2 ns deltas are run-to-run noise everywhere in the bench
suite. The `Layer` column is the chapter number of this guide plus
what the row requires: `core` (no `std`), `alloc`, `std`, `cold`
(diagnostics cold path), `contended` (saturated bench lane). A row must
never disagree with the chapter map above.

| Operation | Layer | Cost / Footprint | When to Use |
|---|---|---|---|
| `snub_check!(cond, reason)` | 1 / core | single branch | Universal invariant check, returns `Err` |
| `SnubberGuard::new` | 1 / core | stack move + owned-field drop | Single function synchronous rollback |
| `EpochGate::observe` | 1 / core | 2-D generation + monotonic epoch filter | Monotonic ordering & reboot re-handshake |
| `classify_stall(wall, cpu, wb, cb)` / `classify_stall_ceiling(.., ceiling)` | 1.5 / core | const math, no clock reads | SPIN vs SLEEP vs DEADLOCK verdict from one sample pair (+ silence ceiling) |
| `WorkerPulse::kick` + `PulseWatch::observe` | 1.5 / core | 8 B, 1 atomic store | Heartbeat: missed pulse = de-energize past thread, never kill it |
| `SystemClock` (`MonotonicMs`) | 1.5 / `std` | 1 `Instant` read | Boot-relative monotonic ms; NTP/DST edits never move deadlines |
| `node.cpu_budget_ms(n)` + `node.stall_ceiling_ms(n)` + `guard.with_cpu_sampler(f)` | 1.5 / std | 1 fn call at arming | CPU-spin budget + silence ceiling; `check_timeout` settles spin/deadlock distinctly |
| `guard.with_cpu_sampler_shared(Arc<dyn Fn() -> u64 + Send + Sync + UnwindSafe + RefUnwindSafe>)` | 1.5 / std | +16 B on the guard (304 → 320); caller's `Arc` is the only allocation | Same verdicts for per-thread state (thread handle, worker counter); wins over the plain `fn` when both are attached |
| `guard.stall_kind()` | 1.5 / std | 1 clock + 1 sample | Non-destructive hang verdict (no settle) |
| `guard.check_timeout()` | 1.5 / std | 1 clock + 1 sample | Cooperative watchdog: `WithinBudget` / `TimedOut` / `SpinDetected` / `DeadlockSuspected` |
| `snubber::watchdog::poll_rollback_node(&node)` | 1 / std | 1 pass over phase atomics, no lock/alloc | Outside verdict: a rollback body older than the node's own `rollback_budget_ms`, with `epoch`/`task_id`; books nothing |
| `snubber::watchdog::poll_rollback(&node, budget_ms)` | 1 / std | same read, caller's threshold | 0.6 shape, kept for callers that outrank the builder; cannot manufacture age |
| `node.rollback_budget_ms()` | 1 / std | 1 field read | The armed budget, so the watcher and the builder cannot disagree |
| `node.rollback_phase()` | 1 / std | 7 relaxed atomic loads (+1 clock read only while a body is armed), no lock/alloc | Live rollback view: `active`, `epoch`, `task_id`, `elapsed_ms`, `budget_ms`, `max_ms`, `stalls`, `reentries` |
| `node.builder(..).rollback_budget_ms(n)` | 1 / std | 2 atomic writes per body; 1 clock read only when `n > 0` | Arms the rollback clock; off by default, so `abort`/drop stays clock-free |
| `RollbackReentrancy::{Allow, Refuse}` + `.rollback_reentrancy(..)` | 1 / std | 1 relaxed load per admission; TLS marker read only while a body is active | Policy for an admission attempted from this node's own rollback thread: `Refuse` returns `EnterError::RollbackReentrant` before any lock |
| `IsrBridge::try_push/pop` + `trip/take_trips` + `IsrSignal` | 1.5 / core | 80 B, ~2 bus cycles | ISR signal into main loop; trip latch bypasses a full ring; no mutex/alloc/`unsafe` (M0-safe byte atomics); `head`/`tail` separated by 64 B cells, no `align(64)` pads |
| `IsrSignal::new(opc, reason, value)` | 1.5 / core | 4 B, const | Pack `OP_EMERGENCY_TRIP` / `OP_PULSE` / `OP_PAUSE` |
| `IdlePark::park` (`SpinPark`/firmware `wfi`/`wfe`) | 1.5 / core | structural: 0% CPU by design | 0%-CPU park: `Condvar` on `std`, `wfi`/`wfe` impl on firmware (`wfe` needs `SEVONPEND=1` at boot) |
| `WdtSink::feed` / `NoWdt` | 1.5 / core | structural: no-op on std | Stroke HW WDT on firmware (same `kick()` feeds it) |
| `node.note_pulse_stall(lag_ms)` | 1.5 / std | structural: 1 atomic max | Record missed pulse into `SnapshotV1` (pulse fields since v3; frame is v6) + `AnomalyKind::PulseStall` |
| `node.note_memory_kb(used_kb)` + `memory_verdict()` | 3 / std | 1 lock + 1 atomic store | External RSS sample → `MemoryVerdict` (Ok/Soft/Hard); admissions refuse lock-free above soft |
| `MemoryWatch` + `MemoryVerdict` | 4 / core | 32 B, `Copy` | Verdict math for firmware; `hard < soft` clamps up |
| `LeakWatch<N>::push(sample)` + `growth_crosses(growth, trip)` | 4 / core | 96 B on 64-bit at `N = 10` (88 B on 32-bit), `Copy`, no clock | Leak as a derivative: `Some(newest − oldest)` only on a full, strictly rising window; one dip clears it |
| `LeakWatch<N>::window()` / `classify(trip, interval_ms)` + `LeakClassification` | 4 / core | one pass over ≤ `N` samples, no allocation | Names the shape instead of a bare `Option`: `Warming` / `Stable` / `Rising { growth_kb, rate_kb_sec }` / `Sawtooth` (the leak a monotonic trip can never see) |
| `MemoryTrend<N>::sample(used_kb)` + `classification()` | 4 / core | 32 B level watch + the ring; the level outranks the trend | One ring, one verdict: `Hard`/`Soft` stay themselves, a rising window under both limits is `MemoryTrendVerdict::Leaking { growth_rate_kb_sec }`; `level()` keeps the `MemoryVerdict` half |
| `HandleWatch<N>::sample(probe)` / `note(count)` | 4 / core | the ring plus two `u64`s, no clock, no allocation | Handle/fd/socket leak by derivative; an unreadable probe returns `None` and records **no** sample (a `0` would be a false dip that disarms the detector) |
| `MemoryPolicy::new(soft, hard)` + `note(used)` | 4 / std | pure integer compares | Supervisor verdict: soft drains, hard kills; `death_cause()` names `MemoryLimit` |
| `Supervisor::run_supervised_guard_with_memory(..)` | 4 / std | 1 sampler call / 10 ms tick | Lease + RSS watchdog; soft logs `MEMORY_DRAIN`, hard `kill()` without draining |
| `node.transition(&mut s, next, rb, work)` | 2 / std | one enum write; zero flag drift | ONE enum state machine, no flag drift |
| `node.transition_checked(s, rb, work)` + `LegalMove` | 2 / std | compile-time only (E0277) | Closed graphs: illegal edges rejected at compile (E0277) |
| `node.enter_with_priority(prio)` | 2 / std | same band as plain enter | Critical over Background yields cooperatively |
| `Protected::new(res, guard)` + `commit()` | 1 / core | 0 ns (generics) | Resource burns with guard unless committed |
| `node.scoped_sealed(rb, work)` + `ScopeToken` | 2 / std | same as `scoped` | `!Send` seal: thread-leak fails at compile |
| `StaticNode::<I, Q>::build(name)` | 2 / std | same as builder | Capacities validated FIRST in `build()` |
| `gateway::<T>(cap)` + `host.recv()` | 2 / std | send+receive roundtrip (a bare `try_send` is cheaper — the roundtrip is what the bench row measures) | Multiple senders, one receiver channel; HWM/LWM throttle paces at 75%/50% with integer `retry_after_ms`, `send_critical` bypasses |
| `RestartTracker::note_death(code, now)` | 4 / std | pure math | Process respawn backoff tracker, caller sleeps |
| `StateDigest` + `take_state()` | 4 / core | 64 B, copy | Hot handoff: memory survives death |
| `NoOrphanGuard` / `OrphanGuard` | 4 / std | trait only | Parent death = child processes reaped (app impl) |
| `node.enter_auto` | 2 / std | atomic sequence ticket minting | Monotonic auto sequence ticket minting |
| `enter_auto_profiled` + `checkpoint_timed` + `commit_profiled` | 2 / std | 1 `Box` at arm + 1 clock read/stage | Stage wall/CPU breakdown (opt-in; default path untouched) |
| `enter_async_auto_profiled` | 3 / std | 1 `Box` + 2 clock reads | Profiled async admission; arms on the admitting poll |
| `guard.profile_snapshot()` | 2 / std | 1 wall + 1 CPU read | Mid-flight profile without settling |
| `ConflateSender::pair()` + `publish` / `take_latest` | 4 / std | queue 0 or 1, replacement counted | Latest-value channel (queue 0 or 1), coalescing counted, no `Clone` bound on payload |
| `ExecutionEnvelope::new(..)` + `execution_envelope(env)` + `commit_enveloped(bytes)` | 4 / std | 16 B `Copy` header; settle judged at commit | Corridor catches too slow/too big and too fast/too small; underflow/overflow in own stats columns, breaker untouched |
| `ExecutionEnvelope::pad_to_floor(us)` / `pad_payload_to_floor(bytes)` | 4 / core | one compare per call, `const` | Raise a value to the declared floor, never shorten one; for padding a WAIT (pacer, coalescing window), never the reported duration; the ceiling is untouched |
| `ExecutionEnvelope::{off, is_off, is_inside, has_underflow, has_overflow, time_verdict, payload_verdict}` | 4 / std | `const fn` header math on one `Copy` value | Inspect a corridor without admitting: `off()`/`is_off()` disable it, `is_inside()` answers the four-bound question, `time_verdict(wall_us)` / `payload_verdict(bytes)` classify one measurement into `TimeVerdict` / `PayloadVerdict` |
| `ProfileHarness` + `HarnessReport` / `run_boundary_harness` | 4 / std | one `Vec` of samples + sort | Deterministic PRNG, min/median/max/jitter/burst matrix, `report.envelope(headroom)`; accessors: `HarnessReport::mean_us()`, `ProfileHarness::{samples, burst_len}`; `FastPrng` is the xorshift32 payload generator behind `run` |
| `snapshot.diff(&later)` + `write_summary` | 5 / cold | pure `Copy` math | Counter deltas + memory/queue window (`SnapshotDiff`) |
| `write_triage(&mut out, node, &snap)` | 5 / cold | `fmt::Write`, no alloc | One ASCII verdict card per node |
| `node_passthrough_enter` | 2 / std canary | lock-free count + unstamped dummy guard | Open-doors transit: lock-free count + unstamped dummy guard, no slots/breaker/`EpochGate`/clock |
| `node_passthrough_scoped` | 2 / std canary | same transit as passthrough enter | Same transit through `scoped()` — work runs, books stay clean |
| `contended_node_enter_threads_6` (6 threads, shared `enter_auto` admit counter) | 2 / std contended | 6-thread saturation | MESI cache-line bouncing on node mutex; StaleEpoch losers bounded-retry (cap 64 + yield), same contract as `concurrency_stress` |
| `contended_node_scoped_threads_6` (6 threads) | 2 / std contended | 6-thread saturation | Full transactional cycle under contention (entry + work + auto-commit) |
| `contended_gateway_mpsc_threads_6` (6 producers + drain) | 2 / std contended | 6-thread saturation | Bounded multi-producer write under parallel core flood |
| `cross_thread_isr_bridge_push_pop` (two threads) | 1.5 / core | two-thread handoff, no affinity pinning | Lock-free SPSC ring between two OS threads (honest two-thread handoff latency, not an L1-hot loop; the benchmark pins neither thread to a core) |
| `node.enter_async_auto` | 3 / std | 0% CPU waker park | Async admission under bounded concurrency |
| `guard.settle_async(fut)` | 3 / std | zero lock during await | Commit across `.await` points |
| `TaskConduit` / `TaskHandle` | 2 / std | lock-free atomics | 2-way live steering, pause/resume waker, drafts |
| `PipeDrain` + `ProcessToken` | 4 / std | 44-byte stream | Hardware rollback across process death (`kill -9`) |
| `ProcessToken::commit(task)` / `from_bytes` / `to_bytes` | 4 / core | `commit` is `const` | 44-byte wire frame, no allocation; `commit(task)` builds the settle-clean marker (`OP_COMMIT`, zero payload) without touching the wire codec |
| `node.snapshot_v1()` + `encode()` | 5 / cold | 512 B, CRC32 | Crash-safe persistence, app-owned flush thread |
| `find_anomalies()` / `assert_no_anomalies!` | 5 / cold | pure `u128` math | CI gate + diagnostics console + `[DIAG 0x…]` bug reports |
| `NodeSet` set | 5 / cold | explicit `&Node` list + saturating aggregates | Multi-node pulse, never a `static` |
| `RankedSet<CAP>::insert(RankedNode::<R>::new(node))` | 5 / cold | fixed `[Option<RankedEntry>; CAP]`, one backward shift per insert, no allocation | Teardown order pinned by `const RANK` (higher stops first, ties stable); a full set refuses rather than evicting a stop step |
| `FlightTape<N>::record_code(epoch, task_id, code)` | 5 / core | 24 B per cell in a fixed array, no allocation, no clock | Last `N` events before a crash; a full tape overwrites its oldest cell, `last_with_code` walks newest first, `N = 0` records nothing |
| `SafeLockExt` (`Mutex::safe_lock(&node)`) | 1 / std | structural: 1 counter on recovery | Poison-tolerant locking: foreign panic recovers the lock, `poison_recoveries` counts each recovery |
| `ClosurePolicy::new(closure)` + `run()` | 2 / alloc | 1 `Box` at construction | Capturing self-healing command: `run()` executes the boxed closure outside the node lock |
| `TransactionHeader { generation, epoch, task_id }` | 1 / core | 24 B `Copy` | Stamped header for trust boundaries; `task_id` doubles as the reply-correlation check |
| `GatewayStats` | 2 / std | plain `Copy` counters | Gateway channel books: `sent` / `admitted` / `shed` / `throttled` (saturating) |
| `ResumeFuture` | 3 / std | 0% CPU waker park | Future resolved by the next directive (pause/resume/cancel); reads the directive under the wakers lock, never duplicates a payload |
| `PulseTick` (type alias) | 1.5 / core | structural: one atomic | Internal tick storage type behind `WorkerPulse` (a private detail surfaced through `WorkerPulse::observe`): `AtomicU64`, or `AtomicU32` wrapping at 2^32 on targets without 64-bit CAS. Not a public API — drive pulses through `WorkerPulse` |
| `EnvelopeVerdict`, `crc32_ieee`, `diagnostic_hash`, `scan_anomalies_for_snapshot`, `find_anomalies_for_snapshot`, `write_diagnostic_line`, `write_health` (Layer 5 helpers) | 5 / std (`find_anomalies_for_snapshot`: alloc) | `Copy` verdict / pure fns / `Write` renderers | Frame-envelope verdict types and the anomaly-scan/hash/render helpers the pool and diagnostic layers are built on; exposed for harnesses that reuse the same classification outside the crate |
| `LeaseEvent { task_id, stage, kind }` | 3 / std | `Copy` event | Reaper mailbox payload fired by `AsyncLease` drop; overflow accounting goes through `LeaseChannel::fire` |
| `run_supervised_guard_with_grace(worker_flag, lease_ms, eof_grace_ms, on_dead_token)` | 4 / std | one reaped child process | Supervised guard with a configurable terminal EOF grace: a slow-flushing child (big buffered stdout) gets more than the default grace before death classification, so an in-flight rescue token is never misreported CLEAN |
| `ProcessToken` / `SnubScanner` (core `drain` module) | 4 / core | 44-byte wire frame; zero allocation | Token parser reachable without `std`: `ProcessToken::commit` / `to_bytes` / `from_bytes` / `from_body_bytes` and the streaming `SnubScanner::push` work on bare metal |
| `PipeDrain::feed` / `finish` / `write_to` (transport) | 4 / std | `feed`: caller-pushed chunks, no reader thread; `write_to` needs `std::io::Write` | Chunk transport layered ON the core scanner: `feed` takes raw bytes (UART/DMA adapters call it directly), `write_to` requires a `std::io::Write` sink — on bare metal implement the adapter yourself |
| `lane.attach(..)` / `lane.note_death(..)` → `PoolReap` | 4 / std | one lane lock each | One reap: `class` (`Ambush`/`Escape`/`Stillborn`/`Deadlocked`), rescued `token`, next `generation`, last `StateDigest`, per-lane `RestartDecision` (`respawn_delay_ms()`); the app owns sleep + spawn |
| `PoolConfig::{with_memory, with_eof_grace}` + `WorkerPool::new` | 4 / std | `const` builder; one `Vec<Arc<PoolLane>>` | Pool configuration: per-lane soft/hard RSS limits and the terminal EOF grace (default `DEFAULT_EOF_GRACE_MS`); `new` validates capacity and returns `PoolError` |
| `PoolLane::{next_epoch, pid, counters, idle_ms, expired, memory_verdict, snapshot}` | 4 / std | one short lane lock per call | Lane introspection: next generation ticket (handshake value), current pid, `DrainCounters`, idle time, lease expiry, last `MemoryVerdict`, and a `LaneSnapshot` row |
| `WorkerPool::total_rescued` / `PoolReport::{deadlocked, conserved, healthy, final_generations}` | 4 / std | saturating fold over lane snapshots; one `Vec<u64>` | Pool-wide rescue count; `deadlocked()` = any lane had an unverifiable pipe; `conserved()` = every lane books its reaps; `healthy()` = both; `final_generations()` = the generation each lane stopped at (`kills + 1` on a clean storm) |
| `pool.report()` / `PoolTotals::of(&lanes)` | 4 / std | four short lane locks + one fold | Aggregated row per lineage (`LaneSnapshot`) plus pool-wide sums; `report()` performs one `Vec<LaneSnapshot>` allocation (`lanes.len()` rows), included in the measurement |
| `pool.expired_lanes(lease_ms)` | 4 / std | one state lock per lane | Lease sweep: names only lanes whose parsed heartbeat froze past the lease; the app kills them itself |
| `LaneSnapshot` / `PoolTotals` | 4 / std | `Clone` row / `Copy` totals | Rows carry `conserved()` (`ambush+escape+stillborn+deadlocks == reaps`); totals fold the same four honest buckets, never a guessed fifth |
| `DEFAULT_EOF_GRACE_MS` (250 ms) | 4 / std | published const, compile-asserted | Default terminal EOF grace; `run_supervised_guard_with_grace` and the pool builder override it per call/site |
| `Supervisor::run_supervised_guard_with_lease_argv(..)` / `..._with_memory_argv(..)` | 4 / std | explicit `&[String]` argv | Argv twins of the lease/RSS watchdogs for tests, embedded launchers and shells that parsed their own command line (index 0 is the program name) |
| `DrainCounters { frames, arms, commits }` | 4 / std | `Copy`, updated per parsed frame | Drain-side frame census: `commits > 0` means the worker settled at least once; `arms > commits` means it was armed more often than it settled (`OP_SNAPSHOT` frames are diagnostics, never counted as arms) |
| `TokenRollback` (`fn(u32, [u8; TOKEN_PAYLOAD_LEN])`) | 4 / std | bare fn pointer | Emergency handler invoked in the supervisor when a dead child leaves an armed token; fast, idempotent, lock-free |
| `ProfiledEnterFuture` | 3 / std (opt-in) | 1 `Box` + clock reads | Future returned by `enter_async_auto_profiled`: same waker parking; profile arms on the admitting poll |
| `backpressure_of(&EnterError)` | 2 / std | pure | `Some(Backpressure)` (retry-after + queue depth) for capacity refusals, `None` when retrying cannot help |
| `Backpressure::jittered(pct, salt)` + `jitter_ms(base, pct, salt)` | 2 / core | pure integer math (SplitMix64 finalizer), no RNG, no clock | Spreads retries inside a symmetric ±`pct`% band; same salt → same delay, `salt == 0` → low edge |
| `AimdLimit::new(init, min, max, step, keep_pct)` + `on_success` / `on_pressure` | 2 / core | 16 B, two integer ops per window | Ceiling that finds the sustainable limit: `+step` per clean window, keep `pct`% per pressure window, floored at `min` |
| `GatewaySender<T>` / `GatewayHost<T>` | 2 / std | sender `Clone`; host unique | Channel gateway endpoints from `gateway(capacity)`: signed depth books, HWM/LWM throttle; the host runs the consumer loop |
| `TOKEN_MAGIC` / `SNAPSHOT_MAGIC` | 4 + 5 / core | `[u8; 4]` (`SNUB`) | Wire and snapshot magic constants. Same literal, separate contracts: token framing vs the 512-byte snapshot header |
| `LANE_SHIFT` (48) / `TASK_LOCAL_MASK` | 4 / core | published consts | Task-id packing: `lane << LANE_SHIFT \| (local & TASK_LOCAL_MASK)`; use `namespace_task` / `task_lane` / `task_local` |
| `STATE_LEN` (64) | 4 / core | const, compile-asserted | Size of `StateDigest`; streamed as three `OP_STATE` frames (24 + 24 + 16) |
