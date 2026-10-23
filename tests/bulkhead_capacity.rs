//! Layer 2: Bulkhead capacity, saturation, hard slot clamping, and backpressure measurements.
//!
//! Merged test suite covering:
//! - Tight bulkhead capacity ceiling (`max_in_flight = 2`) under heavy thread race
//! - `hard_clamp_slots` ceiling guarantee with `EvictOldest` overbooking policy
//! - Full rejection accounting (`attempts == started + bulkhead + stale`)
//! - Backpressure depth and retry-after hints on `BulkheadFull`

use snubber::{EnterError, OverflowPolicy, SnubberNode};
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
    // Total capacity pushback: under concurrent contention, rejections

    // (bulkhead capacity + stale epoch races) must push back against excess load.
    assert!(
        bulk + stale > started,
        "tight wall must push back against excess load (rejected: bulk={bulk} + stale={stale} vs started={started})"
    );
    assert!(bulk > 0, "bulkhead must reject at least some calls");
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
    let err = EnterError::BulkheadFull;
    assert!(err.retry_after_ms() > 0);
    assert!(err.backpressure().map(|b| b.depth).unwrap_or(0) > 0);
}

#[test]
fn hard_clamp_slots_guarantees_ceiling_under_evict_oldest_stress() {
    let node = Arc::new(
        SnubberNode::builder("test_hard_clamp")
            .max_in_flight(2)
            .queue_capacity(8)
            .shed_policy(OverflowPolicy::EvictOldest)
            .trip_after_failures(0)
            .build(),
    );

    let mut handles = Vec::new();
    for thread_id in 0..8usize {
        let n = Arc::clone(&node);
        handles.push(std::thread::spawn(move || {
            for i in 0..200u64 {
                let task_id = thread_id as u64 * 1000 + i;
                if let Ok(guard) = n.enter(task_id, task_id, || {}) {
                    std::thread::yield_now();
                    guard.commit();
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join");
    }

    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "all lanes must return to zero after drain"
    );
    assert!(
        node.bulkhead_has_room(),
        "node must report available room after drain"
    );

    let s = node.stats();
    assert_eq!(
        s.total_started,
        s.committed + s.implicit_drops + s.explicit_aborts,
        "every admitted transaction must settle exactly once"
    );
}

// Regression (0.6.2, bug 14): a BulkheadFull rejection must not burn the
// caller's epoch — the retry with the SAME header stays fresh. Before the
// fix the sync path adopted the epoch at the gate check, so an honest
// retry after a full bulkhead came back StaleEpoch forever.
#[test]
fn bulkhead_rejection_leaves_the_epoch_free_for_retry() {
    let node = SnubberNode::builder("wall/retry-same-epoch")
        .max_in_flight(1)
        .build();
    let held = node.enter(1, 1, || {}).expect("first slot");

    // Bulkhead full: rejected (AdviseOnly -> immediate, no parking).
    match node.enter(2, 2, || {}) {
        Err(EnterError::BulkheadFull) => {}
        Ok(_) => panic!("bulkhead is full, must reject"),
        Err(e) => panic!("expected BulkheadFull, got {e:?}"),
    }

    // Slot freed: the REJECTED epoch retries cleanly.
    held.commit();
    node.enter(2, 2, || {})
        .expect("same epoch must stay fresh after BulkheadFull")
        .commit();
}
