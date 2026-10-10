# snubber

[![Crates.io](https://img.shields.io/crates/v/snubber.svg)](https://crates.io/crates/snubber)
[![Documentation](https://docs.rs/snubber/badge.svg)](https://docs.rs/snubber)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)
[![Tests](https://img.shields.io/badge/tests-125%20passed-brightgreen.svg)]()

> **Transactional safety fuse and boundary shim for Rust.**  
> Guarantees that if your function fails — through a panic, a `?` early return, or process death — your hardware, database transactions, or memory resources automatically reset to a safe state via `Drop`.

Zero external dependencies. `#![forbid(unsafe_code)]`. Works everywhere from bare-metal firmware (`core`-only) to high-throughput servers (`std`).

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
* **Pure Iron Latency:** Floor 1 stack guard costs **~0.51 ns**. Full concurrency node processes over **11.8M ops/sec**.
* **Zero External Dependencies:** Built strictly on `core` and `std`. Floor 1 adds **+0 bytes** to executable size; complete node adds only **3.3 KiB** of machine code.
* **Newton's 3rd Law (Backpressure):** Refusals carry physical telemetry (`retry_after_ms`, `depth 0..255`) so callers pace themselves instead of hammering dead hardware.
* **Zero-Runtime Async:** Bounded waker queue at **0% CPU** without pulling in the Tokio runtime or spinning.
* **10,000 OS Kills Tested:** Verified by automated Chaos Monkey surviving 10,000 hard Windows `TerminateProcess` (`kill -9`) executions with zero pipe deadlocks and 100% token rescue.

---

## 🏎️ Benchmark Snapshot

Measured on AMD/x86-64 release build (`cargo bench --bench admission_bench`):

| Operation | Level | Average Latency | Notes |
|---|---|---|---|
| `SnubberGuard::new` + `commit` | Floor 1 (`core`) | **~0.51 ns** | Pure stack move, zero heap allocation |
| `EpochGate::observe` | Floor 1 (`core`) | **~2.57 ns** | 2-D generation + monotonic epoch filter |
| `node.scoped(rollback, work)` | Floor 2 (`std`) | **~80 ns** | Full bulkhead, circuit breaker, auto-commit |
| `enter_async_auto` | Floor 2 (`std`) | **0% CPU** | Pure `core::task::Waker` parking, no runtime deps |

---

## 📖 Documentation

* 📘 **[User Guide (`docs/USER_GUIDE.md`)](docs/USER_GUIDE.md)** — Complete developer guide:
  * **Level 0:** Universal Ergonomics (`scoped`, `scoped_checked`, `snub_check!`, `SnubberError<E>`)
  * **Level 1:** Single function RAII rollback & `EpochGate`
  * **Level 2:** Daemon protection (`SnubberNode`, presets, token buckets, sliding-window breakers)
  * **Level 3:** Runtime-agnostic async (`enter_async_auto`, `settle_async`, `enter_paced_async`, `checkpoint`)
  * **Level 4:** Cockpit Protocol (2-way live telemetry, pause/resume wakers, draft table)
  * **Level 5:** Out-of-process supervisor (`b"SNUB"` framed tokens, `PipeDrain`, generation bumps)

---

## License

Dual-licensed under either:

* MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
* Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.
