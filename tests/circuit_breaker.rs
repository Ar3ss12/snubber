//! Layer 2: Circuit breaker state machine, transitions, probe recovery, and fail-fast accounting.
//!
//! Merged test suite covering:
//! - Closed -> Open -> HalfOpen -> Closed lifecycle transitions
//! - Instant isolation on Panic policy vs failure streak accumulation
//! - Single probe admission in HalfOpen state followed by concurrent flood
//! - Non-streak accounting for planned aborts and operational noise
//! - Console cap error budgeting

use snubber::{BreakerState, EnterError, FaultKind, SnubberNode, TimeoutCheck, TransactionHeader};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

// ============================================================================
// 1. Instant Isolation & Concurrent Probe Recovery
// ============================================================================

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
    static HITS: AtomicU64 = AtomicU64::new(0);
    match node.enter(2, 2, || {
        HITS.fetch_add(1, Ordering::SeqCst);
    }) {
        Err(EnterError::BreakerOpen) => {}
        Err(_) => panic!("wrong refusal kind"),
        Ok(_) => panic!("open breaker must fail fast"),
    }
    assert_eq!(HITS.load(Ordering::SeqCst), 0);
    assert_eq!(node.stats().breaker_rejections, 1);
}

#[test]
fn breaker_streak_trips_then_probe_recovers_under_fire() {
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

    // Half-open admits exactly one probe at a time. First success closes
    // the breaker synchronously in commit() — followers see Closed and admit normally.
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

    // WAVE 2: flood after recovery — all 8 must flow.
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

// ============================================================================
// 2. State Transitions & Policy Verification
// ============================================================================

#[test]
fn breaker_trips_then_half_open_probe_recovers() {
    let node = SnubberNode::builder("breaker")
        .trip_after_failures(2)
        .build();

    // Two consecutive implicit drops -> trip.
    for epoch in [1u64, 2] {
        let _g = node.enter(epoch, epoch, || {}).expect("enter");
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
    let node = SnubberNode::builder("streak")
        .trip_after_failures(2)
        .build();
    node.report(FaultKind::Timeout, 1, 1);
    assert_eq!(node.stats().breaker_state, BreakerState::Closed);
    node.report(FaultKind::Timeout, 2, 2);
    let s = node.stats();
    assert_eq!(s.breaker_trips, 1, "supervisor faults must trip");
    assert_eq!(s.breaker_state, BreakerState::HalfOpen);

    // Operational shedding must NOT feed the streak.
    let calm = SnubberNode::builder("calm").trip_after_failures(2).build();
    calm.report(FaultKind::StaleEpoch, 1, 1);
    calm.report(FaultKind::StaleEpoch, 2, 2);
    assert_eq!(calm.stats().breaker_trips, 0, "lag is not failure");
    assert_eq!(calm.stats().breaker_state, BreakerState::Closed);
}

#[test]
fn compile_time_budget_bakes_cap_into_builder() {
    let budgeted = SnubberNode::with_error_budget::<8>("budgeted");
    let silent = SnubberNode::with_error_budget::<0>("silent");
    let loud = budgeted.build();
    let quiet = silent.build();
    for epoch in [1u64, 2] {
        let _g = loud.enter(epoch, epoch, || {}).expect("enter");
        let _g = quiet.enter(epoch, epoch, || {}).expect("enter");
    }
    assert_eq!(loud.stats().implicit_drops, 2);
    assert_eq!(quiet.stats().implicit_drops, 2);

    let auto = SnubberNode::console_node::<4>("auto");
    {
        let _g = auto.enter(1, 1, || {}).expect("enter");
    }
    assert_eq!(auto.stats().implicit_drops, 1);
    assert_eq!(
        auto.stats().last_fault.as_ref().map(|f| f.node.as_str()),
        Some("auto")
    );
}

#[test]
fn probe_timeout_reopens_breaker_instead_of_wedging_half_open() {
    let node = SnubberNode::builder("breaker/probe_timeout")
        .trip_after_failures(2)
        .timeout_ms(1)
        .max_in_flight(4)
        .build();
    node.report(FaultKind::Timeout, 1, 1);
    node.report(FaultKind::Timeout, 2, 2);
    assert_eq!(node.stats().breaker_state, BreakerState::HalfOpen);

    let probe = node.enter(100, 100, || {}).expect("probe admitted");
    std::thread::sleep(std::time::Duration::from_millis(5));
    match probe.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {}
        Err(other) => panic!("unexpected watchdog verdict: {other:?}"),
        Ok(_) => panic!("1 ms deadline must fire"),
    }
    // A timed-out probe is a probe FAILURE: the breaker re-opens (same
    // contract as an implicit-dropped probe). Before the fix the breaker
    // stayed HalfOpen with the probe flag armed forever — every later
    // admission read "probe in flight" and nothing could ever recover.
    assert_eq!(
        node.stats().breaker_state,
        BreakerState::Open,
        "timed-out probe must re-open, not wedge in HalfOpen"
    );
    node.reset_breaker();
    node.enter(101, 101, || {})
        .expect("probe slot must be released after timeout")
        .commit();
}
