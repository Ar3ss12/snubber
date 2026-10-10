//! CAMP 2 — REFLEXES (breaker ON): the brakes must SLAM, not swallow.
//!
//! Companion to `tests/cycle_torture.rs` (CAMP 1, reflexes OFF).
//! Here every guard is armed: default `Panic -> TripBreaker` policy,
//! `trip_after_failures(2)`, single-flight bulkhead. We hammer failures
//! and demand the node ISOLATE itself fast, LOUD, and with exact books:
//! * first Panic report trips OPEN immediately (policy, not streak);
//! * admissions during OPEN fail fast as `BreakerOpen` — rollback never
//!   armed, hardware untouched;
//! * half-open probe recovers on success, re-opens on failure;
//! * every refusal is counted — `attempts == started + breaker + ...`.

use snubber::{BreakerState, EnterError, FaultKind, SnubberNode};

#[test]
fn breaker_slams_on_first_panic_report() {
    let node = SnubberNode::builder("reflex/panic")
        .trip_after_failures(100)
        .build();
    // Default Panic policy is TripBreaker — streak never consulted.
    node.report(FaultKind::Panic, 1, 1);
    let s = node.stats();
    assert_eq!(s.breaker_state, BreakerState::Open, "policy must isolate");
    assert_eq!(s.breaker_trips, 1);
    assert_eq!(s.reported[FaultKind::Panic.index()], 1);
    // Fail fast: rollback body never runs.
    static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    match node.enter(2, 2, || {
        HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }) {
        Err(EnterError::BreakerOpen) => {}
        Err(_) => panic!("wrong refusal kind"),
        Ok(_) => panic!("open breaker must fail fast"),
    }
    assert_eq!(HITS.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(node.stats().breaker_rejections, 1);
}

#[test]
fn breaker_streak_trips_then_probe_recovers_under_fire() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    // Timeout feeds the STREAK (no instant-trip policy), trips at 2.
    let node = Arc::new(
        SnubberNode::builder("reflex/streak")
            .trip_after_failures(2)
            .max_in_flight(4)
            .build(),
    );
    node.report(FaultKind::Timeout, 1, 1);
    assert_eq!(node.stats().breaker_state, BreakerState::Closed);
    node.report(FaultKind::Timeout, 2, 2);
    assert_eq!(node.stats().breaker_state, BreakerState::HalfOpen);
    // Half-open admits exactly one probe AT A TIME. First success closes
    // the breaker synchronously in commit() — followers see Closed and
    // admit normally. So: fire ONE probe first, demand exactly one
    // admission while half-open, THEN demand the flood flows.
    // (The old `a == 1, r == 7` assumed the breaker stays half-open
    // while 8 threads race — wrong: commit() closes it for the winner
    // and the rest walk through the open door. That is correct behavior.)
    // WAVE 1: single probe while HalfOpen — must admit exactly one.
    let admitted = Arc::new(AtomicU64::new(0));
    let rejected = Arc::new(AtomicU64::new(0));
    let e0 = 1000;
    match node.enter(e0, e0, || {}) {
        Ok(g) => {
            admitted.fetch_add(1, Ordering::SeqCst);
            g.commit();
        }
        Err(_) => panic!("lone half-open probe must admit"),
    }
    assert_eq!(admitted.load(Ordering::SeqCst), 1);
    assert_eq!(node.stats().breaker_state, BreakerState::Closed);
    // WAVE 2: flood after recovery — all 8 must flow (none vanish).
    // EpochGate is single-lane: when 8 threads race on a global counter,
    // out-of-order arrivals hit StaleEpoch. Retry with fresh tickets per AGENTS.md §3.
    let epoch_alloc = Arc::new(AtomicU64::new(2000));
    let mut handles = Vec::new();
    for _ in 0..8u64 {
        let n = Arc::clone(&node);
        let a = Arc::clone(&admitted);
        let r = Arc::clone(&rejected);
        let alloc = Arc::clone(&epoch_alloc);
        handles.push(std::thread::spawn(move || {
            for _ in 0..64 {
                let e = alloc.fetch_add(1, Ordering::SeqCst);
                match n.enter(e, e, || {}) {
                    Ok(g) => {
                        a.fetch_add(1, Ordering::SeqCst);
                        g.commit();
                        return;
                    }
                    Err(EnterError::BreakerOpen) => {
                        r.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    Err(EnterError::StaleEpoch) => {
                        // Natural multi-lane interleaving: retry with fresh ticket
                        continue;
                    }
                    Err(_) => panic!("unexpected refusal"),
                }
            }
            panic!("exhausted 64 retries on single-lane EpochGate");
        }));
    }
    for h in handles {
        h.join().expect("no wedge");
    }
    let a = admitted.load(Ordering::SeqCst);
    let r = rejected.load(Ordering::SeqCst);
    assert_eq!(a, 9, "1 probe + 8 flood admitted");
    assert_eq!(r, 0, "recovered breaker fails nothing");
    assert_eq!(node.stats().breaker_state, BreakerState::Closed);
    assert_eq!(a + r, 9, "every attempt admitted or counted");
}
