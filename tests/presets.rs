//! Preset regression: desktop / embedded / server shape the node sane.

use snubber::{BreakerState, SnubberNode};

#[test]
fn desktop_preset_is_single_flight_with_breaker() {
    let node = SnubberNode::desktop("preset/desktop");
    let _held = node.enter(1, 1, || {}).expect("first slot");
    assert!(node.enter(2, 2, || {}).is_err(), "single flight");
    assert_eq!(node.stats().bulkhead_rejections, 1);
}

#[test]
fn embedded_preset_is_quiet() {
    let node = SnubberNode::embedded("preset/embedded");
    let _ = node.enter(1, 1, || {});
    // No console spam path armed; books still move.
    assert_eq!(node.stats().total_started, 1);
    assert_eq!(node.stats().breaker_state, BreakerState::Closed);
}

#[test]
fn server_preset_is_wide_with_window_breaker() {
    let node = SnubberNode::server("preset/server");
    for epoch in 1..=10u64 {
        let _ = node.enter(epoch, epoch, || {}).map(|g| g.commit());
    }
    assert_eq!(node.stats().committed, 10, "wide bulkhead absorbs burst");
}
