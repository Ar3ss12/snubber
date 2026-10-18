//! Node tests F: industrial floor — watchdog deadline, fallback plan B,
//! token-bucket rate limit, sliding-window trip, metric export.
//! Public API only.

use snubber::{EnterError, FaultKind, SnubberNode, TimeoutCheck};
use std::thread;
use std::time::Duration;

#[test]
fn watchdog_fires_on_hung_guard() {
    let node = SnubberNode::builder("watchdog").timeout_ms(30).build();
    let g = node.enter(1, 1, || {}).expect("enter");
    thread::sleep(Duration::from_millis(60));
    match g.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {}
        _ => panic!("deadline blown must time out"),
    }
    let s = node.stats();
    assert_eq!(s.timeouts_fired, 1);
    assert_eq!(s.reported[FaultKind::Timeout.index()], 1);
    assert_eq!(s.last_fault.map(|f| f.kind), Some(FaultKind::Timeout));
}

#[test]
fn watchdog_quiet_within_budget() {
    let node = SnubberNode::builder("patient").timeout_ms(5000).build();
    let g = node.enter(1, 1, || {}).expect("enter");
    let g = match g.check_timeout() {
        Ok(g) => g,
        Err(_) => panic!("must be within budget"),
    };
    g.commit();
    assert_eq!(node.stats().timeouts_fired, 0);
    assert_eq!(node.stats().committed, 1);
}

#[test]
fn fallback_serves_plan_b_when_open() {
    let node = SnubberNode::builder("plan_b")
        .trip_after_failures(1)
        .fallback(|| 120)
        .build();
    assert_eq!(node.fallback_value(), None, "closed: no fallback needed");
    {
        let _g = node.enter(1, 1, || {}).expect("enter");
    }
    assert_eq!(node.stats().breaker_state, snubber::BreakerState::HalfOpen);
    // Force fully open via a failed probe for the Netflix path.
    node.report(FaultKind::Panic, 2, 2);
    assert_eq!(node.fallback_value(), Some(120), "open: plan B serves");
}

#[test]
fn token_bucket_caps_admissions_per_second() {
    let node = SnubberNode::builder("bucket")
        .rate_per_sec(2)
        .max_in_flight(16)
        .build();
    node.enter(1, 1, || {}).expect("token 1").commit();
    node.enter(2, 2, || {}).expect("token 2").commit();
    match node.enter(3, 3, || {}) {
        Err(EnterError::RateLimited) => {}
        _ => panic!("bucket empty must rate-limit"),
    }
    let s = node.stats();
    assert_eq!(s.rate_rejected, 1);
    assert_eq!(s.bulkhead_rejections, 1);
}

#[test]
fn sliding_window_catches_intermittent_sickness() {
    // fail-ok-fail-ok never trips a streak counter — window must catch it.
    // Window = 4 outcomes: [fail, ok, fail, ok] = 2/4 = 50% -> trip at 4.
    let node = SnubberNode::builder("intermittent_failure")
        .trip_after_failures(1000)
        .failure_rate(4, 50)
        .build();
    for i in 0..4u64 {
        if i % 2 == 0 {
            let _g = node.enter(i + 1, i + 1, || {}).expect("enter");
        } else {
            node.enter(i + 1, i + 1, || {}).expect("enter").commit();
        }
    }
    let s = node.stats();
    assert_eq!(s.window_outcomes, 0, "window closed and reset");
    assert_eq!(s.breaker_trips, 1, "2/4 = 50% must trip");
    assert_eq!(s.breaker_state, snubber::BreakerState::HalfOpen);
}

#[test]
fn metric_samples_export_tags() {
    let node = SnubberNode::builder("export").build();
    node.report(FaultKind::StaleEpoch, 1, 1);
    node.report(FaultKind::StaleEpoch, 2, 2);
    let rows = node.metric_samples();
    let stale = rows
        .iter()
        .find(|r| r.tag == "stale_packets_dropped")
        .expect("stale tag exported");
    assert_eq!(stale.count, 2);
    assert_eq!(stale.kind, FaultKind::StaleEpoch.index());
    assert_eq!(stale.node, "export");
}
