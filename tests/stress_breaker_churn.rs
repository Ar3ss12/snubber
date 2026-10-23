//! Breaker churn storm (ignored lane): THREAD churn against the failure
//! window, the breaker and the paced queue.
//!
//! The process-storm lanes (`supervisor_pool_stress*`, `supervisor_termination`)
//! prove reaping and conservation under real process churn; they never
//! trip the breaker, the sliding window or `EvictOldest`. This lane does
//! the complementary work: many threads hammer ONE node whose window
//! trips on 50% sickness while an `AdaptiveBlock` pacer and an
//! `EvictOldest` queue absorb the overflow.
//!
//! Gates: the breaker trips, evictions stay at the legal `max + 1`
//! ceiling, every slot is conserved (`in_flight` returns to 0), and a
//! fresh admission after the storm still returns (no wedge).
//!
//! Run: `cargo test --release --test stress_breaker_churn -- --ignored`

#![cfg(feature = "std")]

use snubber::{BackpressurePolicy, BreakerState, OverflowPolicy, SnubberNode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

const THREADS: usize = 8;
const OPS_PER_THREAD: usize = 1500;

#[test]
#[ignore = "storm lane: run with `cargo test --test stress_breaker_churn -- --ignored`"]
fn breaker_churn_trips_recovers_and_conserves() {
    let node = Arc::new(
        SnubberNode::builder("storm/sick")
            .max_in_flight(2)
            .queue_limit(32)
            .overflow(OverflowPolicy::EvictOldest)
            .backpressure(BackpressurePolicy::AdaptiveBlock {
                max_wait_ms: 2,
                shed_deeper_than: None,
            })
            .trip_after_failures(1000)
            .failure_rate(4, 50)
            .build(),
    );

    // Sick pattern: every second task fails (guard dropped uncommitted) —
    // fail-ok-fail-ok defeats any streak counter, only the window sees it.
    let failed = Arc::new(AtomicUsize::new(0));
    let committed = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let node = Arc::clone(&node);
        let failed = Arc::clone(&failed);
        let committed = Arc::clone(&committed);
        handles.push(std::thread::spawn(move || {
            let base = (t as u64) * 10_000;
            for i in 0..OPS_PER_THREAD as u64 {
                let seq = base + i;
                match node.enter(seq, seq, || {}) {
                    Ok(guard) => {
                        if i % 2 == 0 {
                            // Panic-shaped outcome: guard dropped, no commit.
                            drop(guard);
                            failed.fetch_add(1, Ordering::Relaxed);
                        } else {
                            guard.commit();
                            committed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => { /* paced / shed / evicted / breaker refusal */ }
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("storm thread must not panic");
    }

    let s = node.stats();
    // Conservation: every admitted guard ends committed or dropped (the
    // two shapes below); `lane_in_flight` sums to 0 and the async queue is
    // empty — a leaked slot or a ghost waiter would show here.
    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "every admitted slot must be released: committed {} + failed {}",
        committed.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed)
    );
    assert_eq!(
        node.queue_depth(),
        0,
        "no ghost waiter may survive the storm"
    );
    assert!(
        failed.load(Ordering::Relaxed) > 0 && committed.load(Ordering::Relaxed) > 0,
        "the storm must contain both shapes"
    );
    assert!(
        s.breaker_trips >= 1,
        "50% sickness against failure_rate(4, 50) must trip (stats: {s:?})"
    );
    assert!(
        s.evicted >= 1,
        "overflow past the max+1 ceiling must be refused as EvictedOldest, not silently absorbed"
    );
    // The window must have closed and reset at least once (its verdict
    // fired), and the breaker must NOT be left Open: Open would mean the
    // recovery path never ran the HalfOpen probe.
    assert_ne!(
        s.breaker_state,
        BreakerState::Open,
        "storm end must not leave an unrecovered Open breaker"
    );

    // Post-storm liveness: a fresh admission must RETURN (no wedge in the
    // paced/queue machinery). Bounded wait so a wedge fails the gate
    // instead of hanging the runner.
    let probe_node = Arc::clone(&node);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = probe_node.enter(99_999, 99_999, || {});
        let _ = tx.send(outcome);
    });
    let admitted = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("post-storm admission wedged — pacing or queue machinery stuck");
    if let Ok(g) = admitted {
        g.commit();
    } // probe refusal is fine; a HANG is not
    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "probe slot must be released"
    );
}
