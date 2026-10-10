//! Async gateway: refuses fast, never sleeps, no runtime inside.
//!
//! [`AsyncGate`] wraps [`SnubberNode`](crate::SnubberNode) and asserts
//! `AdviseOnly` at construction — a node that would park a thread is
//! rejected, not adapted. Refusals map to HTTP outcomes via [`classify`];
//! the sleep lives in YOUR runtime ([`AsyncSleeper`]).

use snubber::{
    AsyncGate, BackpressurePolicy, EnterError, FaultAction, FaultKind, FaultPolicy, GateError,
    LogLevel, SnubberNode,
};

#[test]
fn gate_rejects_parking_nodes_at_construction() {
    // BlockCaller parks the thread — forbidden behind async.
    let parking = SnubberNode::builder("async/parked")
        .policy(
            FaultKind::BulkheadFull,
            FaultPolicy::arm(
                FaultAction::BlockCaller(50),
                LogLevel::Debug,
                "parked for slot",
                "parked_total",
            ),
        )
        .build();
    assert_eq!(
        AsyncGate::new(parking).err(),
        Some(GateError::WouldBlock),
        "BlockCaller must not pass the gate"
    );
    // AdaptiveBlock parks too.
    let adaptive = SnubberNode::builder("async/adaptive")
        .backpressure(BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 50,
            shed_deeper_than: None,
        })
        .build();
    assert_eq!(
        AsyncGate::new(adaptive).err(),
        Some(GateError::WouldBlock),
        "AdaptiveBlock must not pass the gate"
    );
    // AdviseOnly passes.
    let clean = SnubberNode::builder("async/clean").build();
    assert!(AsyncGate::new(clean).is_ok());
}

#[test]
fn gate_admits_and_classifies_every_refusal() {
    use snubber::{backpressure_of, classify, retry_after_ms, Refusal};
    let node = SnubberNode::builder("async/map")
        .max_in_flight(1)
        .trip_after_failures(100)
        .build();
    let gate = AsyncGate::new(node).expect("clean node");
    // Admit covers the single slot.
    let _held = gate.admit(1, 1, || {}).expect("first in");
    // Second bounces as BulkheadFull → Backoff + advised wait.
    match gate.admit(2, 2, || {}) {
        Err(EnterError::BulkheadFull) => {}
        Err(_) => panic!("wrong refusal kind"),
        Ok(_) => panic!("wall must push back"),
    }
    let bulk = EnterError::BulkheadFull;
    assert_eq!(classify(&bulk), Refusal::Backoff);
    assert!(retry_after_ms(&bulk) > 0);
    assert!(backpressure_of(&bulk).map(|b| b.depth).unwrap_or(0) > 0);
    // Breaker → FailFast, stale → Ignore, breach → BadRequest.
    assert_eq!(
        classify(&EnterError::BreakerOpen),
        Refusal::FailFast,
        "sick downstream → fallback"
    );
    assert_eq!(
        classify(&EnterError::StaleEpoch),
        Refusal::Ignore,
        "late packet → skip"
    );
    assert_eq!(retry_after_ms(&EnterError::StaleEpoch), 0);
    assert!(backpressure_of(&EnterError::StaleEpoch).is_none());
    assert_eq!(
        classify(&EnterError::InvariantBreach("cps == 0")),
        Refusal::BadRequest,
        "bad input → 400"
    );
    assert_eq!(
        classify(&EnterError::RateLimited),
        Refusal::Backoff,
        "rate → backoff"
    );
}

#[test]
fn gate_never_sleeps_admission_is_synchronous() {
    // Admission itself never awaits: measure wall time of a refusal —
    // must return in microseconds, not after a park.
    let node = SnubberNode::builder("async/fast").max_in_flight(1).build();
    let gate = AsyncGate::new(node).expect("clean");
    let _held = gate.admit(1, 1, || {}).expect("first");
    let t0 = std::time::Instant::now();
    let _ = gate.admit(2, 2, || {});
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(20),
        "refusal must be fast (no park), took {:?}",
        t0.elapsed()
    );
}

#[test]
fn cancel_closes_slot_honestly() {
    // select!-style cancel: admit, then drop WITHOUT settle (no commit,
    // no abort) — like a future destroyed mid-.await. The slot MUST come
    // home and the books MUST count it as an implicit drop. What this
    // does NOT prove: half-written network state is untouched by that —
    // idempotency lives outside the crate, not in a destructor.
    use std::sync::atomic::{AtomicU64, Ordering};
    static ROLLBACKS: AtomicU64 = AtomicU64::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let node = SnubberNode::builder("async/cancel")
        .max_in_flight(1)
        .build();
    let gate = AsyncGate::new(node.clone()).expect("clean");
    {
        let _g = gate
            .admit(1, 1, || {
                ROLLBACKS.fetch_add(1, Ordering::SeqCst);
            })
            .expect("admitted");
        // Cancel here: guard destroyed without settle.
    }
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        1,
        "cancel runs the rollback exactly once"
    );
    let s = node.stats();
    assert_eq!(s.total_started, 1, "one admission");
    assert_eq!(s.implicit_drops, 1, "cancel counted as implicit drop");
    assert_eq!(s.committed, 0, "nothing committed");
    // Slot is home: next admission passes (no leak).
    gate.admit(2, 2, || {}).expect("slot home").commit();
    assert_eq!(node.stats().committed, 1);
}
