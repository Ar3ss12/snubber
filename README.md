# snubber

[![GitHub](https://img.shields.io/badge/github-source-blue.svg)](https://github.com/Ar3ss12/snubber)
[![Crates.io](https://img.shields.io/crates/v/snubber.svg)](https://crates.io/crates/snubber)
[![docs.rs](https://docs.rs/snubber/badge.svg)](https://docs.rs/snubber)
[![CI](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml/badge.svg)](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)

> **Transactional safety fuse and failure-recovery engine for Rust.** If a
> protected section fails — `panic!`, an early-return `?`, or the process
> being killed — physical pins, database locks and memory settle back to a
> safe state through `Drop`, and the supervising process receives an exact
> post-mortem diagnosis of the failure.

Zero deps · `#![forbid(unsafe_code)]` · MSRV 1.70 · one crate from `core`-only bare metal to multi-threaded `std`.

---

## Quick start (10 seconds)

```toml
[dependencies]
snubber = "0.7"
```

Wrap critical work in **`scoped`**: two closures, rollback and action. It
commits on `Ok` and runs the rollback on `Err`, on `panic!` and on early
return:

```rust,no_run
use snubber::SnubberNode;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let node = SnubberNode::desktop("orders");
    let amount = 4_900;

    let receipt = node.scoped(
        || println!("rollback: refund {amount}"), // runs on Err, on panic!, on early return
        || Ok::<i64, std::io::Error>(amount),     // work: returns Result<T, E>
    )?;

    println!("committed: {receipt}");
    Ok(())
}
```

<p align="center">
  <img src="https://raw.githubusercontent.com/Ar3ss12/snubber/main/assets/demo.gif" alt="snubber Terminal Stress Demo" width="480"/>
</p>

---

## The delta

```rust,ignore
// WITHOUT snubber: the rollback is a second path you maintain by hand
ledger.lock_funds(req);
let outcome = charge(req);                 // panics here -> funds stay locked
if outcome.is_ok() { ledger.commit(); } else { ledger.unlock(); }
```

```rust,no_run
// WITH snubber: the same scope, plus an invariant checked before anything runs
use snubber::{snub_check, SnubberNode};

struct Request {
    amount: i64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let node = SnubberNode::desktop("orders");
    let req = Request { amount: 4_900 };

    let receipt = node.scoped_checked(
        &req,
        |r| { snub_check!(r.amount > 0, "bad amount"); Ok(()) },
        || println!("rollback: unlock funds"),   // emergency rollback: Err, panic!, early return
        |r| Ok::<i64, std::io::Error>(r.amount), // protected work unit
    )?;

    println!("settled: {receipt}");
    Ok(())
}
```

---

## What it guarantees

* **Rollback is structural.** An admitted guard reverts unless it commits — an
  error return and a panic leave through the same door. `scoped_sealed` adds a
  `!Send` seal: leaking a scope across threads fails at compile.
* **It survives the process, not only the function.** A supervised child emits a
  framed 44-byte token when it arms; if it dies (`kill -9`, `TerminateProcess`,
  OOM euthanasia) the parent reads the pipe to EOF and classifies every reap as
  **true ambush**, **clean escape** or **stillborn**, with
  `ambush + escape + stillborn == reaped` gated on every run. A commit settles
  only the arm whose `task` it names, so a frame for one transaction cannot
  erase another transaction's rescue token.
* **Refusals carry telemetry.** Bulkhead, sliding-window breaker and three
  priority lanes answer with `retry_after_ms` and queue depth; shedding is
  counted as silent discard, never a fake success, and under the default
  policies capacity pressure never trips a breaker.
* **Async without a runtime.** `enter_async_auto` parks on `core::task::Waker`:
  0% CPU while waiting, no Tokio or async-std dependency.
* **A hang gets a verdict.** A monotonic clock plus a CPU-sample fold settles a
  hot `loop {}` as `SpinDetected` and a blocked syscall as `TimedOut` —
  identical rollback, distinct diagnosis, no thread killed. One
  `WorkerPulse::kick()` strokes a hardware watchdog on firmware.
* **A slow rollback is named, not silent.** `rollback_phase()` answers
  `active`, `epoch`, `task_id` and — once `rollback_budget_ms` arms the
  clock — `elapsed_ms`, `max_ms`, `stalls`, `reentries` from any thread
  without the node lock. An overrun that returns becomes
  `FaultKind::RollbackStall`; a body that never returns is named by
  `watchdog::poll_rollback`. The body is never killed — detect and report.
* **State outlives the process.** A 64-byte `StateDigest` is a ticket, not
  storage: a worker near its soft memory limit streams its checkpoint through a
  fixed buffer and hands the digest to its successor.
* **Memory and handles are judged by their derivative.** `MemoryTrend<N>` folds a
  level watch (`Ok` / `Soft` / `Hard`) together with a strictly rising sample
  ring and answers both halves at once — `MemoryReport { level, trend }`, so a
  level can never hide the trend; a slow leak is named by
  `LeakClassification::Rising { growth_kb, rate_kb_sec }` long before a
  ceiling. A half-full window that rose without a dip and would cross the mark
  by the time it fills answers `Advisory` early — a slope to log, never a
  latch (`is_rising()` stays false for it). `HandleWatch<N>` applies the same
  rule to handle / fd counts, which RSS cannot see. One dip clears the window,
  and a closed port records no sample instead of a false `0`.
* **The last events before a fault are kept.** `FlightTape<N>` records a fixed
  number of packed events in a plain array — no allocation, no clock — so a
  post-mortem can read the sequence that led to the fault, not only the totals.
  The 512-byte cold frame carries six of those cells itself: fill
  `snapshot.tape` before `encode()` and the file keeps the order, not just the
  counts.
* **Teardown order is a type, not a convention.** `RankedSet<CAP>` takes
  `RankedNode::<RANK>` entries and runs them highest rank first; a full set
  refuses instead of evicting a stop step.
* **Pressure is capped and spread.** `AimdLimit` moves a ceiling by `+step` per
  clean window and `keep%` on pressure, and `Backpressure::jittered(pct, salt)`
  spreads retries inside a deterministic band so clients do not return in one
  millisecond.
* **Health is one fixed frame.** A 512-byte, zero-allocation `SnapshotV1` with
  CRC32 answers whether the system is healthy, and `assert_no_anomalies!()`
  turns that into a CI gate.
* **Impedance is bounded both ways.** A 10 000 msg/s producer into a 60 FPS
  consumer gets a one-slot `ConflateSender` (latest wins, replacements counted,
  backlog 0 or 1), and the 16-byte `ExecutionEnvelope` also catches work that
  returns *too fast* as `envelope_underflows`.
* **N lineages in one process.** `WorkerPool` gives every lane its own
  `PipeDrain`, generation clock, restart tracker and memory watch; a lane whose
  pipe never reaches EOF gets its own bucket (`ReapClass::Deadlocked`) instead
  of being guessed into another.
* **Extensible by contract.** Public enums are `#[non_exhaustive]`, so new
  verdicts are additive; `friction_fn` / `pressure_fn` take capturing closures
  and run outside the node lock; `snub_check_map!` maps breaches into your error
  type.

### Bulkhead Scope

`max_in_flight(N)` bounds **admissions** — and the two erased entries define
what an admission means:

* `probe_erased_auto` commits the admission probe at arm time. Its slot is
  released before your worker runs, so `N` limits concurrent arm attempts,
  not live guarded transactions.
* `enter_erased_holding_auto` keeps the slot booked until `commit()`,
  `abort()` or `Drop`, so `N` bounds live erased transactions. Use it when
  the ceiling is protecting a finite resource.

The kind is in the type: the probe returns `ErasedGuard<ProbeSlot>`, the
holding entry `ErasedGuard<HoldingSlot>`, so a function that needs the
booked slot cannot be handed a released one by mistake.

The sync (`enter_*`) and async (`enter_async_*`) entries hold their slot for
the whole body in both cases.

---

## When to use / when not

* Hardware and interlock sections (relay lines, actuator pins) where a panic
  must not leave an output energized — the `core` surface needs no allocator.
* Per-endpoint or per-tenant admission in a server, replacing hand-rolled
  counters and breakers.
* Supervising child processes you cannot trust to exit cleanly, and workers that
  must hand state to a successor generation.
* Async entry points in a runtime you do not control.
* **Not for:** distributed transactions — settlement is in-process `Drop` plus
  one pipe frame, there is no two-phase commit. Not a lock implementation —
  `SafeLockExt` wraps locks in guard ergonomics, it does not replace `Mutex`. Not
  a thread scheduler — pool lanes restart children, they do not place threads.

---

## Embedded & `no_std`

`std` and `alloc` are additive features; without `std` the crate is `core`-only
(`#![cfg_attr(not(feature = "std"), no_std)]`). CI builds **both** `core` and
`core + alloc` for `thumbv7em-none-eabihf`. `core`-only targets get the guard,
epoch gate, pulse, ISR bridge, clock, envelope, policy, drain scanner, memory
watch, handle watch and the flight tape; `alloc` upgrades those layers in place
rather than adding one. The `std`
side owns OS threads, pipes and process trees — `SnubberNode`, the async engine,
supervisor, pool, gateway, conduit, conflate and harness. Format ceilings (3
priority lanes, 4 profile stages, 16 ISR slots, 28 B token payload, 512 B
snapshot, 250 ms EOF grace) are published constants, not tunables.

`PipeDrain` is `std` because it owns EOF state and counters, so on a `std`
transport push bytes from a UART/DMA task and declare EOF yourself; on
`core`-only targets the same bytes go straight into the `core` `SnubScanner`.

```rust,no_run
use snubber::PipeDrain;

// std transport layered on the core scanner
fn app_tick_ms() -> u64 { 0 /* firmware SysTick, ms */ }

fn main() {
    let uart_chunk = [0x53, 0x4e, 0x55, 0x42];
    let drain = PipeDrain::manual_with_clock(app_tick_ms); // ClockFn = fn() -> u64, not a number
    let frames = drain.feed(&uart_chunk);                  // any chunking, any size
    println!("complete frames: {frames}");
    drain.finish();                                        // EOF -> rescue classification
}
```

`ProcessToken::write_to(&mut sink)` and `StateDigest::write_to(&mut sink)`
(`std`-gated) replace `emit_if_supervised` when the transport is not stdout; on
bare metal `to_bytes()` returns the 44-byte frame for your own transport.

---

## Performance

Cost shapes; measured figures live in exactly one place.

| Path | Cost / footprint |
|---|---|
| guard arm + `commit` | stack move + owned-field drop, zero heap allocation |
| node admission (bulkhead + breaker) | one short lock; no clock read unless a lease or token bucket is armed |
| async wait | `core::task::Waker` parking, 0% CPU |
| child-death rescue | one 44 B frame over the pipe, classified at EOF |
| health snapshot | 512 B, zero allocation, CRC32 |

👉 Full 52-operation Criterion matrix, contended group and termination-storm
tables — hardware context and committed artifacts included:
**[BENCHMARKS.md](BENCHMARKS.md)**; signatures and footprints without
measurements: [API & cost quick reference](docs/USER_GUIDE.md#api--cost-quick-reference).

---

## Documentation & layers

* 📊 **[Benchmarks (`BENCHMARKS.md`)](BENCHMARKS.md)** — the single numeric home: latency matrix, contended group, storm tables.
* 🧨 **[Invariants & verification protocol (`docs/INVARIANTS.md`)](docs/INVARIANTS.md)** — formal guarantees, admission conservation, slot-leak proofs, gate taxonomy.
* 📘 **[User Guide (`docs/USER_GUIDE.md`)](docs/USER_GUIDE.md)** — the field manual for the current crate: recipes, traps, per-layer contracts.
  * **Layer 1: Core RAII Guard:** Rollback is structural: an admitted guard reverts unless it is committed. Stack rollback, `EpochGate`, `Protected`, `SafeLockExt` and the universal ergonomics (`scoped`, `scoped_checked`, `snub_check!`).
  * **Layer 2: Concurrency Bulkhead:** Admission is capped; the breaker fails fast instead of queueing. `SnubberNode`, presets, token buckets, sliding-window breakers, priority preemption (`PreemptPolicy`: `BackgroundOnly` by default, opt-in `StrictSenior`), `passthrough` canary transit, external memory limits and opt-in execution profiles.
  * **Layer 3: Runtime-Agnostic Async:** One parked waker per wait; any executor drives it, with no runtime dependency. `enter_async_auto`, `settle_async`, `enter_paced_async`.
  * **Layer 4: Process Supervisor:** The supervisor owns the child process; the drain classifies its wire frames. `PipeDrain` with three drivers (`spawn`, `drain_sync`, `feed` + `finish`), framed tokens, generation bumps, `RestartTracker` backoff, memory euthanasia, preventive rotation with a state hand-off (`Rotation` / `RotationHandoff`), `WorkerPool` and rank-ordered teardown (`RankedNode` / `RankedSet`).
  * **Layer 5: Diagnostics & Health:** One 512-byte snapshot answers whether the system is healthy. `SnapshotV1`, anomaly detection, `SnapshotDiff`, the ASCII triage card and `assert_no_anomalies!()`.

---

## Examples

Every file in [`examples/`](examples) runs with `cargo run --example <name>`.

| # | Example | What it proves |
|---|---|---|
| 01 | [Hardware fuse](examples/01_hardware_fuse.rs) | Zero-allocation physical interlock |
| 02 | [Worker daemon](examples/02_worker_daemon.rs) | Autonomous slot claiming by a background worker |
| 03 | [Scoped database](examples/03_scoped_database.rs) | Concurrency limits and adaptive backpressure |
| 04 | [Async web gateway](examples/04_async_web_gateway.rs) | Runtime-agnostic async entry with pacing metadata |
| 05 | [Task conduit](examples/05_task_conduit_steering.rs) | Bi-directional task control and live telemetry |
| 06 | [Process supervisor](examples/06_process_supervisor.rs) | Surviving `kill -9` of a child |
| 07 | [State handoff](examples/07_state_handoff.rs) | Crash-resilient digest passing to a successor |
| 08 | [Hot standby failover](examples/08_hot_standby_failover.rs) | Zero-framework N+1 spare-slot failover |
| 09 | [Pipeline supervisor](examples/09_pipeline_supervisor.rs) | All resilience layers end to end |
| 10 | [Persist snapshot](examples/10_persist_snapshot.rs) | Crash-safe 512 B frame flush the app owns |
| 11 | [Hang classifier](examples/11_hang_classifier.rs) | `SpinDetected` vs `TimedOut` from one sample pair |
| 12 | [ISR bridge](examples/12_isr_bridge.rs) | Interrupt to guard abort over a lock-free SPSC ring |
| 13 | [Canary differential](examples/13_canary_differential.rs) | Naked `passthrough` leaks vs guarded rescue |
| 14 | [Memory recycle](examples/14_memory_recycle.rs) | Soft-limit drain, hard-limit kill verdict |
| 15 | [Supervised memory loop](examples/15_supervised_memory.rs) | Real supervisor + worker on the memory policy |
| 16 | [Streaming checkpoint](examples/16_streaming_checkpoint.rs) | 16 MiB through a 64 KiB buffer; `--naive` dies |
| 17 | [Impedance bridge](examples/17_impedance_bridge.rs) | Raw stream vs conflation vs paced gateway |
| 18 | [Impedance calibration](examples/18_impedance_calibration.rs) | `ProfileHarness` derives the envelope from samples |
| 19 | [Worker pool](examples/19_worker_pool.rs) | N lineages per pool, `--mixed` and `--lease-ms` storms |
| 20 | [Rollback budget](examples/20_rollback_budget.rs) | A wedged rollback body named from outside the lock |

---

## Where to go next

* **[docs.rs/snubber](https://docs.rs/snubber)** — the generated API reference.
* **[CHANGELOG.md](CHANGELOG.md)** — release history and the audit-batch fix tables.
* **[`.github/workflows/ci.yml`](.github/workflows/ci.yml)** — the gates this
  README quotes: MSRV lanes, bare-metal builds, storm lanes, and the doctest
  pass that compiles the `scoped` snippets shown in this file.

---

## License

Dual-licensed under either:

* MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
* Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.
