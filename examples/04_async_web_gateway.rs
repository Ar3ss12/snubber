//! Pattern 04: Async Web Gateway (Tokio/Axum Integration)
//!
//! Run: `cargo run --example 04_async_web_gateway`
//!
//! Runtime-agnostic async entry without bundling Tokio inside the library:
//! The node admits synchronously, refusals map to actionable outcomes via
//! `classify()` + `retry_after_ms()`. In Tokio/Axum, replace the recorded wait with
//! `tokio::time::sleep(...).await`.

use snubber::{AsyncGate, Refusal, SnubberNode};
use std::time::{Duration, Instant};

fn main() {
    let node = SnubberNode::builder("async/demo")
        .max_in_flight(1)
        .trip_after_failures(100)
        .build();
    let gate = AsyncGate::new(node).expect("AdviseOnly passes");
    // Slot 1: admitted.
    let held = gate.admit(1, 1, || {}).expect("first in");
    // Slot 2: wall pushes back — classify, then honour the advised wait
    // with a real (blocking here; `tokio::time::sleep` in an executor)
    // sleep OUTSIDE any node lock.
    match gate.admit(2, 2, || {}) {
        Ok(_) => panic!("wall must push back"),
        Err(e) => {
            assert_eq!(snubber::classify(&e), Refusal::Backoff);
            let wait = snubber::retry_after_ms(&e).clamp(1, 50);
            let started = Instant::now();
            std::thread::sleep(Duration::from_millis(wait));
            let slept = started.elapsed();
            assert!(
                slept >= Duration::from_millis(wait),
                "advised wait {wait} ms must actually elapse outside the node, saw {slept:?}"
            );
            println!("ASYNC_GATE: refused fast, advised wait={wait}ms, slept outside");
        }
    }
    drop(held);
    println!("ASYNC_GATE: no runtime inside, no node lock held across the wait");
}
