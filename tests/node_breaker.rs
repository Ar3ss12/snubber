//! Node tests B: breaker trip / probe / fail-fast, planned aborts,
//! console-cap knob. Public API only.

use snubber::{BreakerState, EnterError, SnubberNode, TransactionHeader};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[test]
fn breaker_trips_then_half_open_probe_recovers() {
    let node = SnubberNode::builder("breaker")
        .trip_after_failures(2)
        .build();

    // Two consecutive implicit drops -> trip.
    for epoch in [1u64, 2] {
        let _g = node.enter(epoch, epoch, || {}).expect("enter");
        // drop without commit
    }
    let s = node.stats();
    assert_eq!(s.implicit_drops, 2);
    assert_eq!(s.breaker_trips, 1);
    assert_eq!(s.breaker_state, BreakerState::HalfOpen);

    // Half-open admits exactly one probe; its success closes the breaker.
    let probe = node.enter(3, 3, || {}).expect("probe admitted");
    assert!(probe.ack::<()>().header() == TransactionHeader::new(3, 3));
    probe.commit();
    let s = node.stats();
    assert_eq!(s.breaker_state, BreakerState::Closed);
    assert_eq!(s.committed, 1);

    // Normal traffic flows again.
    node.enter(4, 4, || {}).expect("traffic resumes").commit();
    assert_eq!(node.stats().committed, 2);
}

#[test]
fn breaker_open_rejects_without_touching_rollback() {
    let calls = Arc::new(AtomicU32::new(0));
    let node = SnubberNode::builder("fast_fail")
        .trip_after_failures(1)
        .build();

    {
        let _g = node.enter(1, 1, || {}).expect("enter");
    }
    // Trip -> HalfOpen; probe dies implicitly -> back to Open.
    {
        let _probe = node.enter(2, 2, || {}).expect("probe");
    }
    assert_eq!(node.stats().breaker_state, BreakerState::Open);

    // OPEN: enter fails before arming — the rollback body never runs.
    let c = Arc::clone(&calls);
    match node.enter(3, 3, move || {
        c.fetch_add(1, Ordering::SeqCst);
    }) {
        Err(EnterError::BreakerOpen) => {}
        _ => panic!("open breaker fails fast"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(node.stats().breaker_rejections, 1);
}

#[test]
fn explicit_abort_does_not_feed_the_breaker() {
    let node = SnubberNode::builder("planned")
        .trip_after_failures(2)
        .build();
    for epoch in [1u64, 2, 3] {
        let g = node.enter(epoch, epoch, || {}).expect("enter");
        g.abort("user_cancelled");
    }
    let s = node.stats();
    assert_eq!(s.explicit_aborts, 3);
    assert_eq!(s.implicit_drops, 0);
    assert_eq!(s.breaker_trips, 0, "planned aborts never trip");
    assert_eq!(s.breaker_state, BreakerState::Closed);
}

#[test]
fn console_cap_knob_builds_and_records() {
    let node = SnubberNode::builder("quiet").build();
    {
        let _g = node.enter(1, 1, || {}).expect("enter");
    }
    assert_eq!(node.stats().implicit_drops, 1);

    // Capped console node: budget is a compile-time-visible knob.
    let capped = SnubberNode::builder("loud")
        .console_report(true)
        .console_cap(2)
        .build();
    for epoch in [1u64, 2, 3] {
        let _g = capped.enter(epoch, epoch, || {}).expect("enter");
    }
    assert_eq!(capped.stats().implicit_drops, 3);
}

#[test]
fn supervisor_named_panic_feeds_the_breaker_streak() {
    // Timeout has no instant-trip policy: two supervisor-named timeouts
    // must trip via the streak, exactly like two implicit drops.
    // (Panic's DEFAULT policy is TripBreaker — instant isolation, a
    // separate tested path — so the streak test uses Timeout.)
    let node = SnubberNode::builder("streak")
        .trip_after_failures(2)
        .build();
    node.report(snubber::FaultKind::Timeout, 1, 1);
    assert_eq!(node.stats().breaker_state, BreakerState::Closed);
    node.report(snubber::FaultKind::Timeout, 2, 2);
    let s = node.stats();
    assert_eq!(s.breaker_trips, 1, "supervisor faults must trip");
    assert_eq!(s.breaker_state, BreakerState::HalfOpen);
    // Noise kinds must NOT feed the streak.
    let calm = SnubberNode::builder("calm").trip_after_failures(2).build();
    calm.report(snubber::FaultKind::StaleEpoch, 1, 1);
    calm.report(snubber::FaultKind::StaleEpoch, 2, 2);
    assert_eq!(calm.stats().breaker_trips, 0, "lag is not failure");
    assert_eq!(calm.stats().breaker_state, BreakerState::Closed);
}

#[test]
fn compile_time_budget_bakes_cap_into_builder() {
    // MAX_ERRORS is a const generic: 8 lines budget vs 0 (silent).
    let budgeted = SnubberNode::with_error_budget::<8>("budgeted");
    let silent = SnubberNode::with_error_budget::<0>("silent");
    let loud = budgeted.build();
    let quiet = silent.build();
    for epoch in [1u64, 2] {
        let _g = loud.enter(epoch, epoch, || {}).expect("enter");
        let _g = quiet.enter(epoch, epoch, || {}).expect("enter");
    }
    // Both record identically — the budget only gates printing.
    assert_eq!(loud.stats().implicit_drops, 2);
    assert_eq!(quiet.stats().implicit_drops, 2);

    // console_node auto-detects the terminal; stats record either way.
    let auto = SnubberNode::console_node::<4>("auto");
    {
        let _g = auto.enter(1, 1, || {}).expect("enter");
    }
    assert_eq!(auto.stats().implicit_drops, 1);
    assert_eq!(auto.stats().last_fault.map(|f| f.node), Some("auto"),);
}
