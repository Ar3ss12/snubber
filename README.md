# snubber

[![CI](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml/badge.svg)](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/snubber.svg)](https://crates.io/crates/snubber)
[![Documentation](https://docs.rs/snubber/badge.svg)](https://docs.rs/snubber)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)
[![Tests](https://img.shields.io/badge/tests-299%20passed-brightgreen.svg)]()

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
snubber = "0.5"
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

## 🛡️ Architecture & Guarantees

* **Zero-Boilerplate RAII Rollback:** No manual token tracking. Scoped executions auto-commit on success and guarantee resource release via `Drop` on panics, errors, or early returns.
* **Single-Digit-Nanosecond Overhead:** Base stack guard costs **~5.4 ns** with full field drop (no `mem::forget` leak) and adds **+0 bytes** to executable size. Concurrency-limited nodes process over **8.2M ops/sec** (`core`-only / `std`).
* **Strict Determinism:** `#![forbid(unsafe_code)]`, zero external dependencies. Predictable behavior across bare-metal microcontrollers and multi-socket servers alike.
* **Runtime-Agnostic Async:** Bounded waker queue built on `core::task::Waker` consuming **0% CPU** without depending on Tokio, async-std, or background threads.
* **Adaptive Backpressure & Pacing:** Refusals carry physical telemetry (`retry_after_ms`, queue depth 0..255) allowing clients to pace incoming traffic instead of overwhelming dead hardware.
* **Out-of-Process Crash Recovery:** Automated stress harness verifies resilience through 10,000 hard Windows `TerminateProcess` (`kill -9`) executions with zero pipe deadlocks and 100% token rescue. Every reap is classified as **true ambush** (armed token at death), **clean escape** (commit seen) or **stillborn** (killed during startup); a multi-lineage storm runs 4 independent supervisors in parallel with per-lane latency and handle gates.
* **Isolated Error Domains:** `FaultKind::is_catastrophic()` cleanly separates unrecoverable failures (Panic, Timeout, ProcessDied, ImplicitDrop) from routine capacity shedding (bulkhead limits, rate limits, stale epochs). Shedding pressure never trips circuit breakers.
* **Monotonic Hang Clock:** Boot-relative `Instant` (`SystemClock`) — NTP sync, DST changes, and manual edits never move a deadline. Firmware brings its own `SysTick` tick via `MonotonicMs`.
* **SPIN vs SLEEP Classification:** A hot `loop {}` settles as `SpinDetected`, a blocked syscall as `TimedOut` — identical rollback, distinct verdict, no thread ever killed. 8-byte `WorkerPulse` heartbeat strokes the hardware WDT on firmware.
* **Zero-Allocation Telemetry:** 512-byte diagnostic snapshots with CRC32 integrity, zero heap allocations on the hot path, and CI validation via `assert_no_anomalies!()`.
* **Selective Pass-Through (Canary Law):** Builder-sealed `passthrough(true)` runs open-doors transit for differential proof — naked leaks counted lock-free, guarded rescues; wire frame stays 512 B.
* **Impedance Bridge:** A 10 000 msg/s producer feeding a 60 FPS consumer gets a one-slot `ConflateSender` — latest wins, replacements counted, backlog stays at 0 or 1. A 16-byte `ExecutionEnvelope` also guards the other wall: work that returns too fast (stale cache, skipped loop) ticks `envelope_underflows` without feeding the breaker. `ProfileHarness` measures min/median/max/jitter/burst and derives the envelope from real numbers.

---

## 🏎️ Benchmark Snapshot

Measured on AMD Ryzen 5 4600H release build
(`cargo bench --bench admission_bench`, Criterion: 800 ms warmup,
2 s measurement, 50 samples, isolated node per bench; re-measured
2026-10-22 after the zero-leak settlement fix):

| Operation | Architecture Layer | Average Latency | Notes |
|---|---|---|---|
| `SnubberGuard::new` + `commit` | Layer 1 (`core`) | **~5.4 ns** | Stack move + owned-field drop, zero heap allocation |
| `EpochGate::observe` | Layer 1 (`core`) | **~2.6 ns** | 2-D generation + monotonic epoch filter |
| `classify_stall` / `classify_stall_ceiling` | Layer 1 (`core`) | **~2.1 ns** (const math) | `SpinDetected` / `SleepBlocked` / `DeadlockSuspected` from one sample pair |
| `node.enter + commit` | Layer 2 (`std`) | **~121 ns** | Full bulkhead, circuit breaker, auto-commit |
| `node.enter_auto + commit` | Layer 2 (`std`) | **~134 ns** | + atomic sequence minting |
| `node.enter_with_priority(Critical)` | Layer 2 (`std`) | **~119 ns** | Priority lane: −2 ns vs plain enter |
| `node.transition` | Layer 2 (`std`) | **~131 ns** | ONE-enum state machine, zero flag drift |
| `node.transition_checked` + `LegalMove` | Layer 2 (`std`) | same + 0 ns | Closed state graphs: illegal moves fail at compile time (E0277) |
| `enter_erased_auto` | Layer 2 (`alloc`) | **~130 ns** | Boxed rollback for dynamic struct fields |
| `node_passthrough_enter + commit` | Layer 2 (`std` canary) | **~98.4 ns** | Open-doors transit: lock-free count + dummy guard, no slots/breaker/`EpochGate` |
| `node_passthrough_scoped + commit` | Layer 2 (`std` canary) | **~99.6 ns** | Same transit through `scoped()` — work runs, books stay clean |
| `ConflateSender::publish + take_latest` | Layer 2 (`std`) | **~22.5 ns** | One-slot latest wins, no allocation, no `Clone` bound |
| `ConflateSender::publish` (occupied slot) | Layer 2 (`std`) | **~16.3 ns** | Replaced value counted, never queued |
| `guard.commit_enveloped(64)` | Layer 2 (`std`) | **~152 ns** | Payload corridor exact; wall corridor only for profiled guards |
| `enter_async_auto` | Layer 3 (`std`) | **0% CPU** | Pure `core::task::Waker` parking, no runtime deps |
| `enter_async_auto_profiled` | Layer 3 (`std`, opt-in) | 1 `Box` + 2 clock reads | Same waker parking, profile arms on the admitting poll |
| `static_node.enter + commit` | Layer 2 (`std`) | **~120 ns** | Compile-time capacity assert + bulkhead + auto-commit |
| `node.note_pulse_stall(lag_ms)` | Layer 2 (`std`) | **1 atomic max** | Missed heartbeat lands in `SnapshotV1` v3 + `AnomalyKind::PulseStall` |
| `node.note_memory_kb(used)` | Layer 2 (`std`, cold) | 1 lock + 1 atomic store | External memory sample → soft/hard verdict; admissions refuse lock-free above soft |
| `process_token.to_bytes()` | Layer 4 (`core`) | **~7.7 ns** | 44-byte framed wire serialization (`to_le_bytes`) |
| `ProcessToken::from_bytes()` | Layer 4 (`core`) | **~7.3 ns** | 40-byte body parse, no allocation |
| `state_digest.crc()` | Layer 4 (`core`) | **~266 ns** | 64-byte hardware cache line digest verification |
| `PipeDrain::drain_sync` | Layer 4 (`std`) | **~61 ns/frame** | 256 frames: 16.5M frames/s including per-stream drain state |
| `run_supervised_guard_with_memory` | Layer 4 (`std`) | 1 sampler call / 10 ms tick | Soft logs `MEMORY_DRAIN`; hard `kill()` without draining (`DeathCause::MemoryLimit`) |
| `enter_auto_profiled` + `checkpoint_timed` + `commit_profiled` | Layer 2 (`std`, opt-in) | 1 `Box` + 1 clock/stage | Stage wall/CPU breakdown; default `enter*` reads no profile clock |
| `snapshot_v1.encode()` | Layer 5 (`core`) | **~1.13 µs** | 512-byte zero-allocation snapshot CRC32 codec |
| `snapshot_v1()` + `find_anomalies()` | Layer 5 (cold) | **512 B, CRC32** | Zero-allocation telemetry; `assert_no_anomalies!()` CI gate |
| `before.diff(&after)` + `write_triage` | Layer 5 (cold) | pure `Copy` math + `fmt::Write` | Counter deltas since last sample; one ASCII verdict card |
| `memory_limits(soft, hard)` + admission | Layer 2 (`std`) | 1 relaxed load | Over soft → `EnterError::MemoryPressure` without taking the mutex |

### ⚡ Contended Multi-Threaded Latencies (Real Cross-Core Saturation)
Measured with half of available hardware cores active (6 concurrent threads under contention, AMD Ryzen 5 4600H):

| Contended Operation | Concurrency | Measured Latency | Physical Phenomenon |
|---|---|---|---|
| `contended_node_enter` | 6 threads | **~893 ns** | L1 cache-line bouncing across physical cores on node mutex |
| `contended_node_scoped` | 6 threads | **~874 ns** | Full transactional cycle under contention (entry + work + auto-commit) |
| `contended_gateway_mpsc` | 6 threads | **~151 ns** | Bounded multi-producer channel write under parallel core flood |
| `cross_thread_isr_bridge` | 2 threads (inter-core) | **~35.3 ns** | Lock-free SPSC ring across physical core boundaries (bus latency) |

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

Per-lane RSS growth ≤ 0.45 MB on both runners; Linux handle growth 0…+1. The macOS rate is lower because identity verification spawns `ps` for every new pid and process spawn is slower, so free-running kills mostly land after the commit (escape) instead of during startup (stillborn).

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

---

## 📖 Documentation & Architecture Layers

* 📘 **[Invariants & Verification Protocol (`docs/INVARIANTS.md`)](docs/INVARIANTS.md)** — Formal mathematical guarantees, admission conservation, zero slot leak proofs.
* 📘 **[User Guide (`docs/USER_GUIDE.md`)](docs/USER_GUIDE.md)** — Comprehensive architecture manual:
  * **Layer 1: Core RAII Guard:** Stack rollback, `EpochGate`, and universal ergonomics (`scoped`, `scoped_checked`, `snub_check!`).
  * **Layer 1.5: Hang Classification:** Monotonic clock (`MonotonicMs` / `SystemClock`), CPU-spin classifier (`classify_stall`, `TimedOut` vs `SpinDetected`), 8-byte `WorkerPulse` heartbeat + `WdtSink`, ISR bridge (`IsrBridge`, 80 B SPSC ring + 32-bit trip latch, `IsrBridge::trip` bypasses `Full`), idle-park (`IdlePark` / `wfi`/`wfe` on firmware, `wfe` needs `SEVONPEND=1` at boot), pulse black-box (`pulse_stalls` + `PulseStall` anomaly, snapshot v3).
  * **Layer 2: Concurrency Bulkhead:** `SnubberNode`, presets, token buckets, sliding-window breakers, priority preemption, builder-sealed `passthrough` canary transit, external memory limits (`memory_limits` → `note_memory_kb`), and opt-in execution profiles (`enter_auto_profiled` → `commit_profiled`). Impedance bridge: one-slot conflation (`ConflateSender`), 16 B `ExecutionEnvelope` corridors (`commit_enveloped`), and the deterministic `ProfileHarness` that measures the corridor before you promise it.
  * **Layer 3: Async Waker Queue:** Runtime-agnostic async execution (`enter_async_auto`, `settle_async`, `enter_paced_async`).
  * **Layer 4: Process Supervisor:** Out-of-process process supervisor, framed IPC tokens, `PipeDrain`, generation bumps, and memory euthanasia (`MemoryPolicy` → soft drain / hard `DeathCause::MemoryLimit`). The 64 B `StateDigest` is a ticket, not storage: a soft-limit worker must stream its checkpoint through a fixed buffer — `hard - soft` is allocator headroom, not a drain countdown.
  * **Layer 5: Diagnostics & Health:** Zero-allocation telemetry, `SnapshotV1` v4 (memory + queue gauges), anomaly detection (`MemoryGrowth`, `QueueSaturation`), `SnapshotDiff`, ASCII triage card, and CI verification gates.

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

---

## License

Dual-licensed under either:

* MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
* Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.
