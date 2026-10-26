# snubber

[![CI](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml/badge.svg)](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/snubber.svg)](https://crates.io/crates/snubber)
[![Documentation](https://docs.rs/snubber/badge.svg)](https://docs.rs/snubber)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)
[![Tests](https://img.shields.io/badge/tests-421%20passed-brightgreen.svg)]()

<!-- Badge provenance: `bash scripts/check_test_count.sh` (all features, 5 ignored), last verified 2026-10-25. -->

> **Transactional safety fuse and failure-recovery engine for Rust.**  
> Guarantees that if your function fails — through a panic, an early return `?`, or unexpected process termination — physical pins, database locks, and memory resources automatically reset to a safe state via `Drop`.

Zero external dependencies. `#![forbid(unsafe_code)]`. Compatible from bare-metal embedded firmware (`core`-only) to high-throughput multi-threaded servers (`std`).

<p align="center">
  <img src="assets/demo.gif" alt="snubber Terminal Stress Demo" width="480"/>
</p>

---

## ⚡ Quickstart (10 seconds)

Add to `Cargo.toml`:
```toml
[dependencies]
snubber = "0.6"
```

Wrap critical work in **`scoped`** — commits on `Ok`, automatically rolls back on `Err` or `panic!`:

```rust
use snubber::{snub_check, SnubberNode};

let node = SnubberNode::desktop("orders");

let receipt = node.scoped_checked(
    &payment_request,
    |req| {
        snub_check!(req.amount > 0, "amount must be positive");
        Ok(())
    },
    || ledger.unlock_funds(),      // emergency rollback if work fails
    |req| gateway.charge(req),     // protected work unit
)?;
```

---

## 🔌 Embedded & `no_std`

`std` and `alloc` are additive features. Without `std` the crate is `core`-only (`#![cfg_attr(not(feature = "std"), no_std)]`); CI builds **both** `core` and `core + alloc` for `thumbv7em-none-eabihf`, and `WorkerPulse` drops to a 32-bit atomic on targets without 64-bit CAS. The `no_std` surface is the watchdog, ISR bridge, clock, envelope, contract, policy, diagnostics core, memory watch, drain (token parser + `SnubScanner`), and protect layers; `alloc` adds the erased guard and dynamic node-name layers; `SnubberNode` itself and the async engine, supervisor, pool, channel, conduit, conflate, and harness layers are `std` because they own OS threads, pipes, and process trees.

The supervisor parser does not require a reader thread or stdout. `PipeDrain` is `std` (it owns the EOF state and counters); push bytes from a UART/DMA task and declare EOF yourself. On `core`-only targets, the same bytes go straight into the `core` `SnubScanner`:

```rust
// std transport layered on the core scanner
let drain = PipeDrain::manual_with_clock(app_tick_ms); // injectable clock
let frames = drain.feed(uart_chunk);                   // any chunking, any size
drain.finish();                                        // EOF → rescue classification
```

`ProcessToken::write_to(&mut sink)` and `StateDigest::write_to(&mut sink)` (`std`-gated) replace `emit_if_supervised` when the transport is not stdout: no environment lookup, no global lock, errors returned. On bare metal, `to_bytes()` returns the 44-byte frame for your own transport.

---

## 🛡️ Architecture & Guarantees

* **Zero-Boilerplate RAII Rollback:** No manual token tracking. Scoped executions auto-commit on success and guarantee resource release via `Drop` on panics, errors, or early returns.
* **Single-Digit-Nanosecond Overhead:** Base stack guard costs **~5.4 ns** (reproducible: `cargo bench --bench admission_bench -- layer1_core`, 2026-10-25 artifact); commit takes the rollback and consumes the disarmed, allocation-free guard shell (a plain stack move — no heap, no extra wire bytes). A saturated node admits **~1.1M admissions/sec** under 6-thread contention (uncontended admission is several times faster; see the latency table).
* **Strict Determinism:** `#![forbid(unsafe_code)]`, zero external dependencies. Predictable behavior across bare-metal microcontrollers and multi-socket servers alike.
* **Runtime-Agnostic Async:** Bounded waker queue built on `core::task::Waker` consuming **0% CPU** without depending on Tokio, async-std, or background threads.
* **Adaptive Backpressure & Pacing:** Refusals carry physical telemetry (`retry_after_ms`, queue depth 0..255) allowing clients to pace incoming traffic instead of overwhelming dead hardware.
* **Out-of-Process Crash Recovery:** The local stress bench (AMD Ryzen 5 4600H, release build) ran 10,000 hard `TerminateProcess` (`kill -9`) executions in armed-hold mode with zero pipe deadlocks and 100% token rescue; CI gates the multi-lineage POSIX storm on every push/PR (4 lanes × 250 armed-hold kills + 4×500 free-running on ubuntu and macos) plus a 25-kill termination smoke, while the 200-kill single-process harness, the 100-kill PowerShell harness, the Win32 multi-lineage storm and the full `#[ignore]` termination gate run on the weekly schedule or manual dispatch. Every reap is classified as **true ambush** (armed token at death), **clean escape** (commit seen) or **stillborn** (killed during startup); a multi-lineage storm runs 4 independent supervisors in parallel with per-lane latency and handle gates, and `WorkerPool` hosts N isolated lineages inside one process (one private `PipeDrain` per worker).
* **Isolated Error Domains:** `FaultKind::is_catastrophic()` cleanly separates unrecoverable failures (Panic, Timeout, ProcessDied, ImplicitDrop) from routine capacity shedding (bulkhead limits, rate limits, stale epochs). Under the default policies, shedding pressure never trips circuit breakers; a per-kind `FaultAction::BreakerOpen` override can opt in.
* **Monotonic Hang Clock:** Boot-relative `Instant` (`SystemClock`) — NTP sync, DST changes, and manual edits never move a deadline. Firmware brings its own `SysTick` tick via `MonotonicMs`.
* **SPIN vs SLEEP Classification:** A hot `loop {}` settles as `SpinDetected`, a blocked syscall as `TimedOut` — identical rollback, distinct verdict, no thread ever killed. A one-atomic-word `WorkerPulse` heartbeat (8 B on 64-bit, 4 B on 32-bit targets) strokes the hardware WDT on firmware.
* **Zero-Allocation Telemetry:** 512-byte diagnostic snapshots with CRC32 integrity, zero heap allocations on the hot path, and CI validation via `assert_no_anomalies!()`.
* **Selective Pass-Through (Canary Law):** Builder-sealed `passthrough(true)` runs open-doors transit for differential proof — naked leaks counted lock-free, guarded rescues; wire frame stays 512 B.
* **Extensible by Contract:** Public error/classification enums are `#[non_exhaustive]` (0.6.0), so new verdicts are additive — downstream matches must keep a `_` arm, and the crate's own internal matches stay exhaustive. `friction_fn` / `pressure_fn` accept capturing closures stored in `Arc<dyn Fn>` and run outside the node lock; `snub_check_map!` maps invariant breaches into your error type; `NodeName::{Static, Owned}` names nodes at runtime without touching the `Copy` telemetry frames (owned-name nodes export the marker `<owned>` in metric/anomaly rows; read `FaultRecord::node` or `node.name()` for the exact label); supervisor entry points have explicit `_argv` variants (`run_supervised_guard_argv`, `run_supervised_guard_with_lease_argv`, `run_supervised_guard_with_memory_argv`) so embedders that don't own `std::env::args` can pass their own argument vector; format ceilings (3 priority lanes, 4 profile stages, 16 ISR slots, 28 B token payload, 512 B snapshot, 250 ms EOF grace) are published constants.
* **Published Helpers & Constants:** `DrainCounters` (frames/arms/commits per drain), `TokenRollback` (emergency `fn` handler), `ProfiledEnterFuture`, `backpressure_of(&EnterError)`, `GatewaySender`/`GatewayHost`, `TOKEN_MAGIC`/`SNAPSHOT_MAGIC`, `LANE_SHIFT`/`TASK_LOCAL_MASK`, `STATE_LEN` — all re-exported and documented in the API table of `docs/USER_GUIDE.md` (`STATE_LEN`, token/snapshot layouts in their layer chapters).
* **Liveness-Correct Concurrency (0.6.1):** every parked async waiter either gets woken or is deregistered (`EvictOldest` resolves the victim to `QueueFull`, never a zombie); the gateway depth counter balances every send with exactly one take (signed arithmetic — the throttle latch can never wedge on an empty queue); a timed-out half-open probe re-opens the breaker instead of wedging; a new generation re-handshakes even at epoch 0; lane generations bump before the EOF grace window, so successors never inherit a dead child's generation; conduit resume/cancel notifications are issued under the waiter's mutex (no lost wakeups).
* **Audit-Hardened Semantics (0.6.2):** shedding never masquerades as success — `BulkheadFull` defaults to counted silent discard (an explicit per-kind `FaultAction::BreakerOpen` still sheds immediately); erased-entry probes settle with the guard, not at admission; a bulkhead rejection no longer burns the epoch (retry works); watchdog timeouts feed the streak breaker; `IsrBridge::try_pop` drains malformed cells; supervisor logs state intent (`LINEAGE_BUMP`, `no_kill_self_drain_expected`) instead of implying actions the one-shot call never takes.
* **Impedance Bridge:** A 10 000 msg/s producer feeding a 60 FPS consumer gets a one-slot `ConflateSender` — latest wins, replacements counted, backlog stays at 0 or 1. A 16-byte `ExecutionEnvelope` also guards the other wall: work that returns too fast (stale cache, skipped loop) ticks `envelope_underflows` without feeding the breaker. `ProfileHarness` measures min/median/max/jitter/burst and derives the envelope from real numbers.

---

## 🏎️ Benchmark Snapshot

Measured on AMD Ryzen 5 4600H release build
(`cargo bench --bench admission_bench`, Criterion: 800 ms warmup,
2 s measurement, 50 samples, isolated node per bench; re-measured
2026-10-24, values from the second of two full runs — the first differed
by under 5% outside the contended group, which moves by design; the
`enter_erased_auto` row comes from two filtered A/B reruns after the
single-box refactor). The four `contended_multithreaded` rows were
re-measured on 2026-10-25 with the fail-fast pool (the 6-thread gateway
row moved from ~172 ns as a result).

**Provenance.** Both tables are **historical snapshots** from those two
full 50-sample runs on the listed machine — they are not re-derived on
every release and move with hardware, compiler and load. The command
above regenerates the whole table into `target/criterion/*/new/estimates.json`;
a 2026-10-26 smoke re-run (`--sample-size 10`) reproduced every bench
group on the same machine. The scheduled `bench-full` CI lane runs
`cargo bench --locked --bench admission_bench`, then
`bash scripts/bench.sh --skip-run --github` to render
`target/bench/snapshot.md` — generated output inside the gitignored
`target/` tree, not a file in this repository — and archives that file
plus the raw Criterion trees as artifact `bench-criterion-<run_id>`. To re-print the table from an
existing `target/criterion` without running benches again, use
`bash scripts/bench.sh --skip-run`. Differences below ~2 ns are inside
run-to-run noise and are not claimed as wins.

| Operation | Architecture Layer | Average Latency | Notes |
|---|---|---|---|
| `SnubberGuard::new` + `commit` | Layer 1 (`core`) | **~5.4 ns** | Stack move + owned-field drop, zero heap allocation |
| `EpochGate::observe` | Layer 1 (`core`) | **~2.6 ns** | 2-D generation + monotonic epoch filter |
| `classify_stall_ceiling` (bench; `classify_stall` is the 4-arg wrapper that delegates here) | Layer 1 (`core`) | **~2.1 ns** (const math) | `CpuSpin` / `SleepBlocked` / `DeadlockStall` from one sample pair (the settle verdicts `SpinDetected` / `DeadlockSuspected` come from `guard.check_timeout`) |
| `node.enter + commit` | Layer 2 (`std`) | **~125 ns** | Full bulkhead, circuit breaker, auto-commit |
| `node.enter_auto + commit` | Layer 2 (`std`) | **~130 ns** | + atomic sequence minting |
| `node.enter_with_priority(Critical)` | Layer 2 (`std`) | **~126 ns** | Priority lane; same band as plain enter (one measurement) |
| `node.transition` | Layer 2 (`std`) | **~126 ns** | ONE-enum state machine, zero flag drift |
| `node.transition_checked` + `LegalMove` | Layer 2 (`std`) | same path, no extra work | Compile-time only: the `LegalMove` proof erases to the same `transition` call; illegal moves fail at compile (E0277) |
| `enter_erased_auto` | Layer 2 (`alloc`) | **~208 ns** | ONE boxed transaction (rollback + commit hook + node clone in a single allocation; 0.6.8 moved the breaker outcome onto the guard's real `commit`). 301.6 → 208 ns after folding two boxes and two `Arc` clones into one |
| `node_passthrough_enter + commit` | Layer 2 (`std` canary) | **~97.1 ns** | Open-doors transit: lock-free count + dummy guard, no slots/breaker/`EpochGate` |
| `node_passthrough_scoped + commit` | Layer 2 (`std` canary) | **~94.8 ns** | Same transit through `scoped()` — work runs, books stay clean |
| `ConflateSender::publish + take_latest` | Layer 2 (`std`) | **~21.6 ns** | One-slot latest wins, no allocation, no `Clone` bound |
| `ConflateSender::publish` (occupied slot) | Layer 2 (`std`) | **~16.0 ns** | Replaced value counted, never queued |
| `guard.commit_enveloped(64)` | Layer 2 (`std`) | **~142 ns** | Payload corridor exact; wall corridor only for profiled guards |
| `enter_async_auto` | Layer 3 (`std`) | **0% CPU** | Pure `core::task::Waker` parking, no runtime deps |
| `enter_async_auto_profiled` | Layer 3 (`std`, opt-in) | structural: 1 `Box` + 2 clock reads | Same waker parking, profile arms on the admitting poll |
| `enter_async_auto` (ready poll) | Layer 3 (`std`) | **~152 ns** | Full admission through the future; the ready path allocates nothing |
| `enter_async_auto` park/wake/admit roundtrip | Layer 3 (`std`) | **~546 ns** | Same-thread park (queue push + identity flag) -> slot release wake -> admitting re-poll; no executor, not a cross-core roundtrip |
| `static_node.enter + commit` | Layer 2 (`std`) | **~127 ns** | Compile-time capacity assert + bulkhead + auto-commit |
| `node.note_pulse_stall(lag_ms)` | Layer 2 (`std`) | structural: 1 atomic max | Missed heartbeat lands in `SnapshotV1` (pulse fields since v3; frame is v5) + `AnomalyKind::PulseStall` |
| `node.note_memory_kb(used)` | Layer 2 (`std`, cold) | structural: 1 lock + 1 atomic store | External memory sample → soft/hard verdict; admissions refuse lock-free above soft |
| `process_token.to_bytes()` | Layer 4 (`core`) | **~8.0 ns** | 44-byte framed wire serialization (`to_le_bytes`) |
| `ProcessToken::from_bytes()` | Layer 4 (`core`) | **~7.4 ns** | 40-byte body parse, no allocation |
| `state_digest.crc()` | Layer 4 (`core`) | **~262 ns** | 64-byte hardware cache line digest verification |
| `PipeDrain::drain_sync` | Layer 4 (`std`) | **~44 ns/frame** | 256 frames: 11.2 µs, 22.8M frames/s; the per-sample `PipeDrain` construction is included, so this is a pipeline cost, not a steady-state frame cost (see `feed` below) |
| `PipeDrain::feed` (one 44 B frame) | Layer 4 (`std`) | **~98 ns** | Caller-pushed chunk: scanner + lock + dispatch, no reader thread |
| `PipeDrain::feed` (256 frames/chunk) | Layer 4 (`std`) | **~10.8 µs** (42.3 ns/frame) | Same scanner: one ingest lock per chunk + one short state lock per token (0.6.8 took the clock read outside the lock) |
| `ProcessToken::write_to(&mut sink)` | Layer 4 (`std`) | **~2.1 ns** | 44 B `write_all` into any real `Write` (copy included): no env, no stdout lock |
| `StateDigest::write_to(&mut sink)` | Layer 4 (`std`) | **~625 ns** | Three `OP_STATE` frames + CRC32 seals |
| `namespace_task` pack / unpack | Layer 4 (`std`) | **~1.0 / 1.0 ns** | `task = lane << 48 \| local`; two shifts + mask, no branch |
| `lane.attach` + `note_death` (ambush) → `PoolReap` | Layer 4 (`std`) | **~192 ns** | Full rescue path: drain EOF, token take, generation bump, restart math, per-class counters (drain built in setup; the shared idle lane is re-attached each sample) |
| `lane.attach` + `note_death` (escape) → `PoolReap` | Layer 4 (`std`) | **~173 ns** | Same path when the child settled (commit parsed, no token) |
| `pool.report()` (4 lanes) | Layer 4 (`std`) | **~111 ns** | Rows (`LaneSnapshot`) + `PoolTotals::of`: four short lane locks, totals fold, one `Vec<LaneSnapshot>` allocation (`lanes.len()` rows) included in the number |
| `LaneSnapshot` / `PoolTotals` | Layer 4 (`std`) | `Clone` row / `Copy` totals | `conserved()` = `ambush+escape+stillborn+deadlocks == reaps`; no guessed fifth bucket |
| `run_supervised_guard_with_grace(..)` | Layer 3 (`std`) | one reaped child process | Supervised guard with a configurable terminal EOF grace for slow-flushing children |
| `DEFAULT_EOF_GRACE_MS` (250 ms) | Layer 3 (`std`) | published const | Default terminal EOF grace; `run_supervised_guard_with_grace` and the pool builder override it |
| `lane.note_memory_kb` | Layer 4 (`std`, cold) | **~12 ns** | Per-lane watch: one lock + integer compares |
| `pool.expired_lanes(lease)` (4 lanes) | Layer 4 (`std`, cold) | **~517 ns** | One sweep over four silent pipes: one state lock per lane, no child touched |
| `run_supervised_guard_with_memory` | Layer 4 (`std`) | structural: 1 sampler call / 10 ms tick | Soft logs `MEMORY_DRAIN`; hard `kill()` without draining (`DeathCause::MemoryLimit`) |
| `enter_auto_profiled` + `checkpoint_timed` + `commit_profiled` | Layer 2 (`std`, opt-in) | structural: 1 `Box` + 1 clock/stage | Stage wall/CPU breakdown; default `enter*` reads no profile clock |
| `snapshot_v1.encode()` | Layer 5 (`core`) | **~1.13 µs** | 512-byte zero-allocation snapshot CRC32 codec |
| `friction_fn` handler call (`Arc<dyn Fn>`) | Layer 4 (`std`) | **~1.8 ns** | Capture-capable friction: one dynamic dispatch outside the node lock |
| `friction_wait_ms` (adaptive, Arc handler) | Layer 4 (`std`) | **~21.5 ns** | Lock + `Arc` clone + handler; identical to the legacy fn-pointer path (~21.5 ns) |
| `NodeName::Static` clone | Layer 4 (`std`) | **~1.6 ns** | Fat-pointer copy, no allocation |
| `NodeName::Owned` clone | Layer 4 (`std`) | **~10.0 ns** | One `Arc<str>` increment; `as_str` ~1.0 ns |
| `snapshot_v1()` + `find_anomalies()` | Layer 5 (cold) | structural: 512 B, CRC32 | Zero-allocation telemetry; `assert_no_anomalies!()` CI gate |
| `before.diff(&after)` + `write_triage` | Layer 5 (cold) | structural: pure `Copy` math + `fmt::Write` | Counter deltas since last sample; one ASCII verdict card |
| `memory_limits(soft, hard)` + admission | Layer 2 (`std`) | structural: 1 relaxed load | Over soft → `EnterError::MemoryPressure` without taking the mutex |

**Why the shape, not just the numbers.** The User Guide argues the four
ratios this table depends on: the per-layer split is a
[level budget, not a time budget](docs/USER_GUIDE.md#46c-checkpoint-physics-64-b-is-a-ticket-not-storage)
(`hard - soft` is allocator headroom, not a countdown); the 512 B snapshot
is a
[deliberate lossy checkpoint](docs/USER_GUIDE.md#why-a-naive-serialization-dies-50-mb-state)
because cloning 50 MB of state crosses the hard limit before the first
`write_all`; a pre-fork RSS number is not a container number because
[cgroup v2 counts page cache](docs/USER_GUIDE.md#cgroup-v2-page-cache-counts-against-the-limit);
and `EvictOldest` admits exactly one transient
[`max + 1`](docs/USER_GUIDE.md#fixed-in-064-post-release-audit-strike)
slot — beyond that it sheds like every other caller.

### ⚡ Contended Multi-Threaded Latencies (Real Thread Saturation)
Measured with half of available hardware cores active (6 concurrent threads under contention, AMD Ryzen 5 4600H), full 50-sample run of 2026-10-25 (`cargo bench --locked --bench admission_bench contended_multithreaded`; archived as `target/bench/snapshot.md` — generated by `scripts/bench.sh`, not a repository file — together with the raw Criterion trees in the CI `bench-criterion` artifact):

| Contended Operation | Concurrency | Measured Latency | Physical Phenomenon |
|---|---|---|---|
| `contended_node_enter_threads_6` | 6 threads | **~925 ns** | Mutex contention: cache-line transfer between threads |
| `contended_node_scoped_threads_6` | 6 threads | **~899 ns** | Full transactional cycle under contention (entry + work + auto-commit) |
| `contended_gateway_mpsc_threads_6` | 6 threads | **~146 ns** | Bounded multi-producer channel write under parallel thread load |
| `cross_thread_isr_bridge_push_pop` | 2 threads | **~34 ns** | Lock-free SPSC ring: cache-line handoff between threads |

A 2026-10-26 smoke re-run (`--sample-size 10`, same machine) read 1029.5 / 1011.1 / 159.4 / 37.2 ns for the same four rows — the contended group moves between runs by design; quote full-run artifacts, not smoke checks.

### ⚡ Multi-Lineage Termination Storm

`scripts/stress_multi_lineage.ps1` runs N supervisors in parallel, each with its own worker, pid file, pipe and raw Win32 killer. One supervisor = one lineage: isolation needs no library changes because each lane runs in its own working directory. `--armed-hold` parks the worker armed and publishes its pid only after the arm frame, so an ambush-rate gate cannot be satisfied by shooting processes that never armed.

`scripts/stress_multi_lineage.py` is the same harness for Linux and macOS: `SIGKILL` instead of `TerminateProcess`, process identity from `/proc/<pid>/stat` starttime (Linux) or `ps -o lstart` (macOS), so a recycled pid is never mistaken for the live worker. Python 3 standard library only, no dependencies. `.github/workflows/stress.yml` runs both the armed-hold and free-running storms on `ubuntu-latest` and `macos-latest` on every push.

Measured on AMD Ryzen 5 4600H, release build, 4 lanes × 2,500 = 10,000 `TerminateProcess`:

| Mode | Wall | Kills/s (avg / peak) | Dispatch p50 | Ambush | Escape | Stillborn |
|---|---|---|---|---|---|---|
| `-Aggressive -ArmedHold` | 25.60 s | 390.6 / 462.7 | 360 µs | 10,000 (100%) | 0 | 0 |
| `-Aggressive` (free-running) | 13.89 s | 719.8 / 822.2 | 376 µs | 2,023 (20.2%) | 91 (0.9%) | 7,886 (78.9%) |

Armed-hold run: per-lane dispatch p50 354–366 µs, private-memory growth +0.61…+1.01 MB, handle growth 0…+1, final generation = kills + 1 on every lane, no pipe deadlock. Free-running shows what the rate costs: at 720 kills/s most shots land during process startup, not in the armed window.

The same gates run in CI (`.github/workflows/stress.yml`, `--aggressive`, dispatch latency = `SIGKILL`/`TerminateProcess` call only):

| Runner | Mode | Kills | Wall | Kills/s | Ambush | Escape | Stillborn | p50 / p99 |
|---|---|---|---|---|---|---|---|---|
| ubuntu-latest (2 vCPU) | armed-hold | 1,000 | 0.91 s | 1,095 | 100% | 0 | 0 | 5 / 11–14 µs |
| ubuntu-latest (2 vCPU) | free-running | 2,000 | 1.11 s | 1,803 | 93.2% | 2.4% | 4.5% | 4 / 12–17 µs |
| macos-latest (arm64) | armed-hold | 1,000 | 7.19 s | 139.1 | 100% | 0 | 0 | 22–24 / 344–554 µs |
| macos-latest (arm64) | free-running | 2,000 | 31.82 s | 62.9 | 61.9% | 38.1% | 0 | 13–14 / 57–84 µs |

Per-lane RSS growth ≤ 0.45 MB on both runners; Linux handle growth 0…+1 (macOS now samples descriptors with `lsof -p` and gates the same limit). The macOS rate is lower because identity verification spawns `ps` for every new pid and process spawn is slower, so free-running kills mostly land after the commit (escape) instead of during startup (stillborn).

```powershell
# Single-lineage release gate (10,000 kills)
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 10000 -Launch -RawKill -Aggressive
# Armed-hold rescue proof (true ambush >= 70%)
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 1000 -Launch -RawKill -Aggressive -ArmedHold
# 4 independent lineages, 2,500 kills each
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold
```

```bash
# Linux / macOS: same storm, SIGKILL path, JSON gates
python3 scripts/stress_multi_lineage.py --lines 4 --kills-per-line 2500 --aggressive --armed-hold
```

### ⚡ Worker Pool (`WorkerPool`): N lineages in one process

A single `Supervisor` owns one lineage: one armed slot, one `OP_STATE` scratch slot and one blocking `child.wait()`. The pool scales the count instead of merging state — every lane keeps its own `PipeDrain`, `GenerationClock`, `RestartTracker` (backoff + storm window) and `MemoryPolicy`, and one app thread waits on each child. The wire format does not change; `take_armed` still means exactly one worker's last un-settled token.

```rust
use snubber::pool::{namespace_task, PoolConfig, WorkerPool};

let pool = WorkerPool::new(PoolConfig::new(4))?;          // 4 lanes, OnCrash, 50→1000 ms
let lane = pool.lane(0).unwrap().clone();                 // one Arc per app thread
let generation = lane.attach(pid, PipeDrain::spawn(stdout))?;
let reap = lane.note_death(Some(exit_code), now_ms)?;           // waits ≤ 250 ms for EOF
match reap.decision { /* Restart { delay_ms } | Stop | GiveUp */ }
```

`note_death` classifies from that lane's own pipe: **ambush** (token handed back), **escape** (commit seen), **stillborn** (neither). A pipe that does not reach EOF inside the grace window becomes a fourth honest bucket — `ReapClass::Deadlocked`; `PoolReport::healthy()` stays false until it is zero, and `ambush + escape + stillborn + deadlocks == reaps` is checked per lane and in the totals. Task ids can carry the lineage (`namespace_task`, `lane << 48 | local`), so a rescued token names its worker. A waiter blocked in `child.wait()` cannot see a *different* lane go silent, so one app-side monitor calls `pool.expired_lanes(lease_ms)` on a tick: it names only lanes that already proved traffic once, and the monitor kills those itself.

Measured (`cargo test --test supervisor_pool_stress -- --ignored --nocapture`, 4 lanes × 8 hard kills, debug build): 32/32 reaps, 32/32 true ambush, 0 escape, 0 stillborn, 0 deadlocks, final generation `[9,9,9,9]` (= kills + 1 per lane), `wall_ms=55` (2026-10-26, [`docs/evidence/2026-10-26_pool_stress_report.json`](docs/evidence/2026-10-26_pool_stress_report.json)).

The mixed storm cycles `armed → clean → silent → orphan` by `local % 4` inside the same run (`--mixed`, example 19): `cargo test --test supervisor_pool_stress_mixed -- --ignored --nocapture` measured 32/32 conserved reaps, **A/E/S/D = 8/8/8/8**, 8 rescued tokens, `deadlock=true` (the orphan modes hold the pipe open past the 150 ms grace), `[9,9,9,9]` generations, `wall_ms=474` (2026-10-26, [`docs/evidence/2026-10-26_pool_mixed_stress_report.json`](docs/evidence/2026-10-26_pool_mixed_stress_report.json)). A deadlocked lane is reported, never guessed into another bucket.

The lease storm (`--lease-ms`, example 19) is the opposite case: every child emits one heartbeat and then hangs forever, so no `wait()` ever returns on its own. `cargo test --test supervisor_pool_stress_lease -- --ignored --nocapture` measured 24/24 reaps — all **stillborn** (a heartbeat is liveness, not a settlement), 0 ambush, 0 escape, 0 deadlocks, `lease_kills=24`, `conservation_breaks=0` over 99,628 live `report()` samples from the observer thread (2026-10-26, [`docs/evidence/2026-10-26_pool_lease_stress_report.json`](docs/evidence/2026-10-26_pool_lease_stress_report.json), `monitor_checks`), `[7,7,7,7]` generations, `wall_ms=194`. A child that hung before its first heartbeat is not expired — a pipe that never parsed a frame is the app's spawn timeout, not a lease breach.

Every number in the three paragraphs above is copied from the committed reports in [`docs/evidence/`](docs/evidence/README.md): the bucket counters and generations are deterministic, while `wall_ms` and `monitor_checks` are wall-clock observations that move with load and were not hand-picked.

All three storms plus `stress_breaker_churn` run in the `release-ignored` job of `.github/workflows/ci.yml` on every push and pull request (4 lanes × 8 / 6 / 8 kills and 8 threads × 1500 churn ops, ~2 s of test time); the heavier supervisor and multi-lineage harnesses stay scheduled or dispatch-only.

Per-reap cost scales with lanes, not with the pool: `attach + note_death` is ~192 ns (ambush) / ~173 ns (escape), a four-lane `report()` is ~111 ns, a four-lane lease sweep is ~517 ns, and `namespace_task` is ~1 ns — each lane locks only its own state.

---

## 📖 Documentation & Architecture Layers

* 📘 **[Invariants & Verification Protocol (`docs/INVARIANTS.md`)](docs/INVARIANTS.md)** — Formal mathematical guarantees, admission conservation, zero slot leak proofs.
* 📘 **[User Guide (`docs/USER_GUIDE.md`)](docs/USER_GUIDE.md)** — Comprehensive architecture manual:
  * **Layer 1: Core RAII Guard:** Stack rollback, `EpochGate`, and universal ergonomics (`scoped`, `scoped_checked`, `snub_check!`).
  * **Layer 1.5: Hang Classification:** Monotonic clock (`MonotonicMs` / `SystemClock`), CPU-spin classifier (`classify_stall` → `CpuSpin` / `DeadlockStall`; `guard.check_timeout()` → `SpinDetected` / `TimedOut`), one-word `WorkerPulse` heartbeat + `WdtSink`, ISR bridge (`IsrBridge`, 80 B SPSC ring + 32-bit trip latch, `IsrBridge::trip` bypasses `Full`), idle-park (`IdlePark` / `wfi`/`wfe` on firmware, `wfe` needs `SEVONPEND=1` at boot), pulse black-box (`pulse_stalls` + `PulseStall` anomaly, snapshot v5).
  * **Layer 2: Concurrency Bulkhead:** `SnubberNode`, presets, token buckets, sliding-window breakers, priority preemption, builder-sealed `passthrough` canary transit, external memory limits (`memory_limits` → `note_memory_kb`), and opt-in execution profiles (`enter_auto_profiled` → `commit_profiled`). Impedance bridge: one-slot conflation (`ConflateSender`), 16 B `ExecutionEnvelope` corridors (`commit_enveloped`), and the deterministic `ProfileHarness` that measures the corridor before you promise it.
  * **Layer 3: Async Waker Queue:** Runtime-agnostic async execution (`enter_async_auto`, `settle_async`, `enter_paced_async`).
  * **Layer 4: Process Supervisor:** Out-of-process process supervisor, framed IPC tokens, `PipeDrain`, generation bumps, and memory euthanasia (`MemoryPolicy` → soft drain / hard `DeathCause::MemoryLimit`). `PipeDrain` has three drivers over one parser — `spawn` (reader thread), `drain_sync` (blocking `Read`), and `feed` + `finish` (caller-pushed chunks) — plus an injectable `ClockFn` for lease timing and `write_to` sinks for non-stdout transports. The 64 B `StateDigest` is a ticket, not storage: a soft-limit worker must stream its checkpoint through a fixed buffer — `hard - soft` is allocator headroom, not a drain countdown. `WorkerPool` runs N independent lineages (one drain, clock, tracker and memory watch per lane) with per-lane rescue classification and an aggregated report.
  * **Layer 5: Diagnostics & Health:** Zero-allocation telemetry, `SnapshotV1` v5 (memory + queue gauges, per-`FaultKind` counters including `RateLimited`), anomaly detection (`MemoryGrowth`, `QueueSaturation`), `SnapshotDiff`, ASCII triage card, and CI verification gates.

---

## 🧪 Production Patterns

* **[01 Hardware Fuse (`examples/01_hardware_fuse.rs`)](examples/01_hardware_fuse.rs):** Zero-allocation physical interlock safety.
* **[02 Worker Daemon (`examples/02_worker_daemon.rs`)](examples/02_worker_daemon.rs):** Autonomous slot claiming for background workers.
* **[03 Scoped Database (`examples/03_scoped_database.rs`)](examples/03_scoped_database.rs):** Concurrency limits and adaptive backpressure.
* **[04 Async Web Gateway (`examples/04_async_web_gateway.rs`)](examples/04_async_web_gateway.rs):** Runtime-agnostic async entry with pacing metadata.
* **[05 Task Conduit Steering (`examples/05_task_conduit_steering.rs`)](examples/05_task_conduit_steering.rs):** Bi-directional task control and live telemetry.
* **[06 Process Supervisor (`examples/06_process_supervisor.rs`)](examples/06_process_supervisor.rs):** Surviving abnormal child process termination (`kill -9`).
* **[07 State Handoff (`examples/07_state_handoff.rs`)](examples/07_state_handoff.rs):** Crash-resilient successor state digest passing.
* **[08 Hot Standby Failover (`examples/08_hot_standby_failover.rs`)](examples/08_hot_standby_failover.rs):** Zero-framework N+1 standby spare slot failover.
* **[09 Pipeline Supervisor (`examples/09_pipeline_supervisor.rs`)](examples/09_pipeline_supervisor.rs):** Full end-to-end integration of all resilience layers.
* **[10 Persist Snapshot (`examples/10_persist_snapshot.rs`)](examples/10_persist_snapshot.rs):** Crash-safe 512 B frame flush the app owns (tmp write + rename); the crate never touches files.
* **[11 Hang Classifier (`examples/11_hang_classifier.rs`)](examples/11_hang_classifier.rs):** App-side CPU clock backend; hot `loop {}` settles as `SpinDetected`, blocked sleep as `TimedOut` — no thread is ever killed.
* **[12 ISR Bridge (`examples/12_isr_bridge.rs`)](examples/12_isr_bridge.rs):** Hardware interrupt to guard abort via lock-free SPSC ring (80 B, ~2 bus cycles per push) — timer pulse feeds `WorkerPulse`, overcurrent trip latches a bit that bypasses a full ring.
* **[13 Canary Differential (`examples/13_canary_differential.rs`)](examples/13_canary_differential.rs):** Naked `passthrough(true)` leaks vs guarded rescue on a virtual driver — comparative proof stand, zero real hardware moves.
* **[14 Memory Recycle (`examples/14_memory_recycle.rs`)](examples/14_memory_recycle.rs):** Soft limit → drain → `StateDigest` handoff to a successor generation; hard limit → `MemoryPolicy` kill verdict.
* **[15 Supervised Memory Loop (`examples/15_supervised_memory.rs`)](examples/15_supervised_memory.rs):** Real supervisor + worker processes on `run_supervised_guard_with_memory` — soft logs the drain window, hard euthanizes without draining (`DeathCause::MemoryLimit`).
* **[16 Streaming Checkpoint (`examples/16_streaming_checkpoint.rs`)](examples/16_streaming_checkpoint.rs):** 16 MiB body through a 64 KiB stack buffer (`tmp` → `sync_all` → `rename` → seal → successor restore); `--naive` clones the state, crosses hard and is killed mid-`write_all` with no sealed file and no ticket.
* **[17 Impedance Bridge (`examples/17_impedance_bridge.rs`)](examples/17_impedance_bridge.rs):** One 10 000 msg/s producer through a raw stream (unbounded backlog), a one-slot conflation channel (latest wins, coalescing counted) and a paced gateway (HWM/LWM with `retry_after_ms`) at 60 FPS.
* **[18 Impedance Calibration (`examples/18_impedance_calibration.rs`)](examples/18_impedance_calibration.rs):** `ProfileHarness` measures min/median/max/jitter/burst, derives an `ExecutionEnvelope` with headroom, then a node judges real settlements — including a deliberate no-op that lands below the floor.
* **[19 Worker Pool (`examples/19_worker_pool.rs`)](examples/19_worker_pool.rs):** N independent lineages in one pool — one `PipeDrain`, generation clock, restart tracker and memory watch per lane; every child is killed only after its decisive frame is parsed, and the JSON report gates `reaped`, `ambush`, `deadlock` and `final_generation`. `--mixed` cycles ambush/escape/stillborn/deadlock in one storm; `--lease-ms` kills workers that beat once and then hang, using `expired_lanes` from a monitor thread.

---

## License

Dual-licensed under either:

* MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
* Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.
