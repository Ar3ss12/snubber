# snubber

[![CI](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml/badge.svg)](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/snubber.svg)](https://crates.io/crates/snubber)
[![Documentation](https://docs.rs/snubber/badge.svg)](https://docs.rs/snubber)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)
[![Tests](https://img.shields.io/badge/tests-188%20passed-brightgreen.svg)]()

> **Transactional safety fuse and boundary shim for Rust.**  
> Guarantees that if your function fails — through a panic, a `?` early return, or process death — your hardware, database transactions, or memory resources automatically reset to a safe state via `Drop`.

Zero external dependencies. `#![forbid(unsafe_code)]`. Works everywhere from bare-metal firmware (`core`-only) to high-throughput servers (`std`).

<p align="center">
  <img src="assets/demo.gif" alt="snubber Chaos Monkey Terminal Demo" width="480"/>
</p>

---

## ⚡ Quickstart (10 seconds)

Add to `Cargo.toml`:
```toml
[dependencies]
snubber = "0.1"
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

## 🛡️ Why snubber?

* **Zero-Boilerplate Rollback:** No manual token tracking. Scoped executions auto-commit on success and trigger RAII cleanup on any failure.
* **Pure Iron Latency:** Floor 1 stack guard costs **~1.5 ns**. Full concurrency node processes over **13.8M ops/sec** (`1e9 / 72.6 ns`).
* **Zero External Dependencies:** Built strictly on `core` and `std`. Floor 1 adds **+0 bytes** to executable size; complete node adds only **3.3 KiB** of machine code.
* **Newton's 3rd Law (Backpressure):** Refusals carry physical telemetry (`retry_after_ms`, `depth 0..255`) so callers pace themselves instead of hammering dead hardware.
* **Zero-Runtime Async:** Bounded waker queue at **0% CPU** without pulling in the Tokio runtime or spinning.
* **10,000 OS Kills Tested:** Verified by automated Chaos Monkey surviving 10,000 hard Windows `TerminateProcess` (`kill -9`) executions with zero pipe deadlocks and 100% token rescue.
* **Nuclear vs Hydraulic Telemetry:** `FaultKind::is_catastrophic()` splits hull-breach faults (Panic/Timeout/ProcessDied/ImplicitDrop) from pressure shedding (bulkhead/rate/stale/drain) — shed never trips the breaker. Flight recorder keeps the last 16 fault codes, passive `TelemetrySink` shouts outside the lock.
* **Cold Diagnostics (zero-dep):** 512 B `SnapshotV1` codec (`SNUB` magic + CRC32, app-owned flush thread — see `examples/persist_snapshot.rs`), pure `find_anomalies()` over cached snapshots, `assert_no_anomalies!()` CI gate, `print_health()` pulse table, `diagnostic_line()` one-liner for bug reports (`[DIAG 0x8FA3 ...]`), explicit `NodeRegistry` board (never a `static`).

---

## 🏎️ Benchmark Snapshot

Measured on AMD Ryzen 5 4600H release build
(`cargo bench --bench admission_bench`, Criterion: 800 ms warmup,
2 s measurement, 50 samples, isolated node per bench):

| Operation | Level | Average Latency | Notes |
|---|---|---|---|
| `SnubberGuard::new` + `commit` | Floor 1 (`core`) | **~1.5 ns** | Pure stack move, zero heap allocation |
| `EpochGate::observe` | Floor 1 (`core`) | **~2.6 ns** | 2-D generation + monotonic epoch filter |
| `node.enter + commit` | Floor 2 (`std`) | **~72.6 ns** | Full bulkhead, circuit breaker, auto-commit |
| `node.enter_auto + commit` | Floor 2 (`std`) | **~86.5 ns** | + atomic sequence minting |
| `node.enter_with_priority(Critical)` | Floor 2 (`std`) | **~73.6 ns** | Priority lane: +1 ns over plain enter |
| `node.transition` | Floor 2 (`std`) | **~86.5 ns** | ONE-enum state machine, no flag drift |
| `node.transition_checked` + `LegalMove` | Floor 2 (`std`) | same + 0 ns | Closed graphs: illegal ribs die at compile (E0277) |
| `enter_erased_auto` | Floor 3 (`alloc`) | **~85.6 ns** | Boxed rollback for struct fields |
| `enter_async_auto` | Floor 2 (`std`) | **0% CPU** | Pure `core::task::Waker` parking, no runtime deps |

---

## 📖 Documentation

* 📘 **[User Guide (`docs/USER_GUIDE.md`)](docs/USER_GUIDE.md)** — Complete developer guide:
  * **Level 0:** Universal Ergonomics (`scoped`, `scoped_checked`, `scoped_sealed` + `ScopeToken`, `transition`, `transition_checked` + `LegalMove`, `StaticNode`, `Protected`, `snub_check!`, `SnubberError<E>`)
  * **Level 1:** Single function RAII rollback & `EpochGate`
  * **Level 2:** Daemon protection (`SnubberNode`, presets, token buckets, sliding-window breakers, `Priority` preemption, `gateway` checkpoint)
  * **Level 3:** Runtime-agnostic async (`enter_async_auto`, `settle_async`, `enter_paced_async`, `checkpoint`)
  * **Level 4:** Cockpit Protocol (2-way live telemetry, pause/resume wakers, draft table)
  * **Level 5:** Out-of-process supervisor (`b"SNUB"` framed tokens, `PipeDrain`, generation bumps, `RestartTracker`, `StateDigest` handoff, `OrphanGuard`)
  * **Level 6:** Cold diagnostics (nuclear vs hydraulic, `app.snub`, `assert_no_anomalies!`, `[DIAG 0x…]`)
* 🧪 **Patterns, not frameworks:** `examples/hot_standby.rs` (N+1 shadow pool), `examples/orphan_guard.rs` (Win32 Job Objects recipe), `examples/cockpit_intercom.rs` (intercom steering)

---

## License

Dual-licensed under either:

* MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
* Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.
