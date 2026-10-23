//! Fault classification: operational shedding vs catastrophic failure handling.
//!
//! Pins the classification contour: `FaultKind::is_catastrophic` is the single
//! source of truth for `report()` streak feeding. `ImplicitDrop` is
//! catastrophic but owned by `settle_implicit` (no double-feed via `report`).
//! Operational shedding (e.g. `BulkheadFull`, `RateLimited`, `StaleEpoch`) never trips the breaker.

use snubber::{BreakerState, FaultKind, SnubberNode, TransactionHeader};

#[test]
fn catastrophic_table_pins_every_variant() {
    let catastrophic_faults = [
        FaultKind::ImplicitDrop,
        FaultKind::Panic,
        FaultKind::Timeout,
        FaultKind::ProcessDied,
    ];
    for kind in FaultKind::ALL {
        let expect = catastrophic_faults.contains(&kind);
        assert_eq!(
            kind.is_catastrophic(),
            expect,
            "contour moved for {kind:?} — update report() + docs together"
        );
    }
}

#[test]
fn shed_never_trips_breaker() {
    let node = SnubberNode::builder("bulkhead_saturation")
        .max_in_flight(1)
        .trip_after_failures(3)
        .rate_per_sec(1)
        .build();
    // Hold the single slot so every enter sheds.
    let _held = node.enter(1, 1, || {}).expect("slot held");
    let mut bulk: u64 = 0;
    let mut rate: u64 = 0;
    let mut stale: u64 = 0;
    for i in 2..5002u64 {
        match node.enter(i, i, || {}) {
            Err(snubber::EnterError::BulkheadFull) => bulk += 1,
            Err(snubber::EnterError::RateLimited) => rate += 1,
            _ => {}
        }
    }
    for i in 9000..9100u64 {
        // Epochs behind the gate: stale cuts.
        if !node.observe(&TransactionHeader::new(1, i)) {
            stale += 1;
        }
    }
    // Operational shedding reports must not feed the streak either.
    for i in 0..10u64 {
        node.report(FaultKind::BulkheadFull, 50_000 + i, i);
        node.report(FaultKind::BufferOverflow, 51_000 + i, i);
        node.report(FaultKind::EvictedOldest, 52_000 + i, i);
        node.report(FaultKind::BreakerOpen, 53_000 + i, i);
        node.report(FaultKind::StaleEpoch, 54_000 + i, i);
        node.report(FaultKind::PoisonedPayload, 55_000 + i, i);
    }
    let s = node.stats();
    assert_eq!(
        s.breaker_state,
        BreakerState::Closed,
        "shed must not isolate"
    );
    assert_eq!(s.breaker_trips, 0);
    assert_eq!(s.catastrophic_faults(), 0, "no catastrophic fault recorded");
    assert!(s.shed_bulkhead() >= bulk);
    assert_eq!(s.shed_rate(), s.rate_rejected);
    assert!(
        s.shed_rate() >= rate,
        "rate rejections must be >= loop hits"
    );
    assert!(s.shed_stale() >= stale);
    assert_eq!(s.poison_recoveries, 0);
}

#[test]
fn catastrophic_faults_trip_breaker() {
    let node = SnubberNode::builder("catastrophic")
        .trip_after_failures(3)
        .build();
    node.report(FaultKind::Panic, 1, 1);
    node.report(FaultKind::Timeout, 2, 2);
    node.report(FaultKind::ProcessDied, 3, 3);
    let s = node.stats();
    assert_eq!(s.catastrophic_faults(), 3);
    assert_eq!(
        s.breaker_state,
        BreakerState::HalfOpen,
        "3 catastrophic reports trip the breaker exactly like the guard path: Closed -> HalfOpen (probe on next enter)"
    );
    assert_eq!(s.breaker_trips, 1);
    assert_eq!(s.poison_recoveries, 0);

    // The HalfOpen probe is scheduled on the next enter.
    node.enter(9, 9, || {}).expect("probe admitted").commit();
    assert_eq!(
        node.stats().breaker_state,
        BreakerState::Closed,
        "probe success closes the breaker"
    );

    // Implicit drops feed the streak via settle (guard path).
    let node2 = SnubberNode::builder("implicit_catastrophic")
        .trip_after_failures(3)
        .build();
    for i in 1..=3u64 {
        let g = node2.enter(i, i, || {}).expect("admit");
        drop(g); // implicit drop: catastrophic via settle_implicit
    }
    let s2 = node2.stats();
    assert_eq!(s2.implicit_drops, 3);
    assert_eq!(s2.catastrophic_faults(), 3);
    assert_eq!(
        s2.breaker_state,
        BreakerState::HalfOpen,
        "3 implicit drops trip breaker to HalfOpen probe"
    );
}

#[test]
fn channel_disconnect_is_operational_shedding() {
    let node = SnubberNode::builder("graceful_drain")
        .trip_after_failures(3)
        .build();
    for i in 0..10u64 {
        node.report(FaultKind::ChannelDisconnected, 100 + i, i);
    }
    let s = node.stats();
    assert_eq!(s.breaker_state, BreakerState::Closed);
    assert_eq!(s.breaker_trips, 0);
    assert_eq!(s.catastrophic_faults(), 0, "drain is not a breach");
    assert_eq!(s.reported[FaultKind::ChannelDisconnected.index()], 10);
    assert_eq!(s.poison_recoveries, 0);
}
