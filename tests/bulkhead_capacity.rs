//! Bulkhead capacity & saturation: capacity limits and rejection accounting.
//!
//! Companion to `tests/concurrency_stress.rs` (wide concurrency).
//! Here the bulkhead is tight (`max_in_flight(2)`, 8 threads): excess
//! attempts are rejected as `BulkheadFull`, and every rejection is
//! counted on both sides (test atomics == node books). What must hold:
//! * `attempts == started + bulkhead` — none vanish;
//! * admitted guards settle exactly once (`started == settled`);
//! * telemetry readings accompany the error (`retry_after_ms > 0`, depth > 0).

use snubber::{EnterError, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn bulkhead_pushes_back_and_counts_every_bounce() {
    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static BULK: AtomicU64 = AtomicU64::new(0);
    static STALE: AtomicU64 = AtomicU64::new(0);
    static SETTLED: AtomicU64 = AtomicU64::new(0);
    ATTEMPTS.store(0, Ordering::SeqCst);
    STARTED.store(0, Ordering::SeqCst);
    BULK.store(0, Ordering::SeqCst);
    STALE.store(0, Ordering::SeqCst);
    SETTLED.store(0, Ordering::SeqCst);
    let node = Arc::new(
        SnubberNode::builder("reflex/wall")
            .max_in_flight(2)
            .queue_limit(65536)
            .trip_after_failures(0)
            .build(),
    );
    let epoch = Arc::new(AtomicU64::new(1));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let n = Arc::clone(&node);
        let e = Arc::clone(&epoch);
        handles.push(std::thread::spawn(move || {
            for _ in 0..500u64 {
                let ep = e.fetch_add(1, Ordering::SeqCst);
                ATTEMPTS.fetch_add(1, Ordering::SeqCst);
                // Hold the guard a breath so the wall actually bites.
                match n.enter(ep, ep, || {}) {
                    Ok(g) => {
                        STARTED.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_micros(200));
                        g.commit();
                        SETTLED.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(EnterError::BulkheadFull) => {
                        BULK.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(EnterError::StaleEpoch) => {
                        // Global-epoch race: count the race separately,
                        // retry once fresh.
                        STALE.fetch_add(1, Ordering::SeqCst);
                        let ep2 = e.fetch_add(1, Ordering::SeqCst);
                        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
                        match n.enter(ep2, ep2, || {}) {
                            Ok(g) => {
                                STARTED.fetch_add(1, Ordering::SeqCst);
                                g.commit();
                                SETTLED.fetch_add(1, Ordering::SeqCst);
                            }
                            Err(EnterError::BulkheadFull) => {
                                BULK.fetch_add(1, Ordering::SeqCst);
                            }
                            Err(EnterError::StaleEpoch) => {
                                STALE.fetch_add(1, Ordering::SeqCst);
                            }
                            Err(_) => {
                                BULK.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                    Err(other) => panic!("unexpected refusal: {other:?}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("no wedge");
    }
    let s = node.stats();
    let attempts = ATTEMPTS.load(Ordering::SeqCst);
    let started = STARTED.load(Ordering::SeqCst);
    let bulk = BULK.load(Ordering::SeqCst);
    let stale = STALE.load(Ordering::SeqCst);
    let settled = SETTLED.load(Ordering::SeqCst);
    println!(
        "\n[WALL]: attempts={attempts} started={started} settled={settled} bulk={bulk}(node={}) stale={stale}(node={})",
        s.bulkhead_rejections,
        s.stale_epochs_rejected
    );
    // The wall bit: with 2 slots vs 8 hammering threads, most bounced.
    assert!(bulk > started, "tight wall must reject most attempts");
    assert_eq!(started, s.total_started, "test count matches node books");
    assert_eq!(bulk, s.bulkhead_rejections, "every bounce counted");
    assert_eq!(stale, s.stale_epochs_rejected, "every race counted");
    assert_eq!(
        attempts,
        started + bulk + stale,
        "no attempt vanished at the wall"
    );
    assert_eq!(settled, started, "every admission settled once");
    assert_eq!(
        settled, s.committed,
        "wall test commits everything it admits"
    );
    // Readings ride the refusal (Newton's 3rd law, info layer).
    let err = EnterError::BulkheadFull;
    assert!(err.retry_after_ms() > 0);
    assert!(err.backpressure().map(|b| b.depth).unwrap_or(0) > 0);
}
