//! Stage 2 (PLAN_AUDIT2 F-02, decision D1): the sync queue horizon is
//! telemetry-only. The horizon fires on an admission that still SUCCEEDS,
//! so it must never execute `policy.action` — no breaker trip, no alert
//! latch, no custom command. The async `RejectNew` queue-full refusal is a
//! genuine rejection and keeps the full policy. `queue_capacity(0)` has
//! asymmetric semantics: sync = horizon fires on every admission, async =
//! waker queue floored at 1.

use core::task::{Context, Poll, Waker};
use snubber::{
    BreakerState, EnterError, FaultAction, FaultKind, FaultPolicy, LogLevel, SnubberNode,
};
use std::future::Future;
use std::sync::Arc;
use std::task::Wake;

fn trip_buffer_overflow() -> FaultPolicy {
    FaultPolicy::arm(
        FaultAction::TripBreaker,
        LogLevel::Warn,
        "horizon overflow",
        "buffer_overflow_total",
    )
}

fn rollback_buffer_overflow() -> FaultPolicy {
    FaultPolicy::arm(
        FaultAction::RollbackAndAlert,
        LogLevel::Warn,
        "horizon overflow",
        "buffer_overflow_total",
    )
}

fn fake_waker(id: usize) -> Waker {
    struct FakeWake(usize);
    impl Wake for FakeWake {
        fn wake(self: Arc<Self>) {
            let _id = self.0;
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let _id = self.0;
        }
    }
    Waker::from(Arc::new(FakeWake(id)))
}

/// `queue_capacity(0)` makes the horizon comparison `queued >= queue_limit`
/// true on every admission while a guard is live. That is the documented
/// meaning of zero: the gauge reports every admission, nothing is refused.
#[test]
fn horizon_over_zero_limit_never_trips_the_breaker() {
    let node = SnubberNode::builder("horizon/trip")
        .max_in_flight(4)
        .queue_capacity(0)
        .policy(FaultKind::BufferOverflow, trip_buffer_overflow())
        .build();
    for epoch in [1u64, 2, 3] {
        let g = node
            .enter(epoch, epoch, || {})
            .expect("horizon is a gauge, not a refusal");
        g.commit();
    }
    let s = node.stats();
    assert_eq!(s.breaker_trips, 0, "a successful admission must not trip");
    assert_eq!(s.breaker_state, BreakerState::Closed);
    assert_eq!(
        s.reported[FaultKind::BufferOverflow.index()],
        3,
        "gauge still exported once per admission"
    );
    assert_eq!(s.bulkhead_rejections, 0, "no shed column for telemetry");
    assert_eq!(
        s.admission_total(),
        s.total_started + s.shed_total(),
        "conservation law intact"
    );
}

#[test]
fn horizon_rollback_and_alert_policy_does_not_latch_the_alert() {
    let node = SnubberNode::builder("horizon/alert")
        .max_in_flight(4)
        .queue_capacity(0)
        .policy(FaultKind::BufferOverflow, rollback_buffer_overflow())
        .build();
    for epoch in [1u64, 2] {
        let g = node.enter(epoch, epoch, || {}).expect("admitted");
        g.commit();
    }
    assert!(
        !node.take_alert(),
        "RollbackAndAlert on BufferOverflow must not latch the UI alert for a successful admission"
    );
    assert_eq!(node.stats().reported[FaultKind::BufferOverflow.index()], 2);
}

#[test]
fn horizon_still_exports_the_metric_sample() {
    let node = SnubberNode::builder("horizon/metric")
        .max_in_flight(4)
        .queue_capacity(0)
        .build();
    for epoch in [1u64, 2] {
        let g = node.enter(epoch, epoch, || {}).expect("admitted");
        g.commit();
    }
    let count: u64 = node
        .metric_samples()
        .iter()
        .filter(|r| r.kind == FaultKind::BufferOverflow.index())
        .map(|r| r.count)
        .sum();
    assert_eq!(count, 2, "buffer_overflow_total visible in metric_samples");
}

/// Contrast: the async `RejectNew` queue-full refusal IS a rejection, so the
/// same `TripBreaker` policy armed on `BufferOverflow` must trip there.
#[test]
fn async_queue_full_refusal_keeps_full_policy() {
    let node = SnubberNode::builder("horizon/async")
        .max_in_flight(1)
        .queue_capacity(1)
        .policy(FaultKind::BufferOverflow, trip_buffer_overflow())
        .build();
    let _g = node.enter(1, 1, || {}).expect("slot 1");

    let w = fake_waker(1);
    let mut cx = Context::from_waker(&w);
    let mut f1 = Box::pin(node.enter_async(2, 2, || {}));
    assert!(f1.as_mut().poll(&mut cx).is_pending(), "waiter parked");

    let mut f2 = Box::pin(node.enter_async(3, 3, || {}));
    match f2.as_mut().poll(&mut cx) {
        Poll::Ready(Err(EnterError::QueueFull)) => {}
        _ => panic!("expected QueueFull"),
    }

    let s = node.stats();
    assert_eq!(s.breaker_trips, 1, "a genuine refusal keeps the policy");
    assert_eq!(s.breaker_state, BreakerState::Open);
    assert_eq!(s.bulkhead_rejections, 1, "refusal books the shed column");
}

/// Async `queue_capacity(0)` is a documented asymmetry: the waker-queue
/// capacity is floored at 1 (`q_cap = queue_limit.max(1)`), so exactly ONE
/// waiter may park and a second parked attempt is refused immediately with
/// `QueueFull` (RejectNew). Sync semantics differ: there `0` means the
/// horizon fires on every admission — see
/// `horizon_over_zero_limit_never_trips_the_breaker`. This test pins the
/// current behavior; the full field split is deferred (PLAN_AUDIT2 F-07,
/// P3).
#[test]
fn async_zero_capacity_allows_exactly_one_waiter_then_queue_full() {
    let node = SnubberNode::builder("horizon/async-zero")
        .max_in_flight(1)
        .queue_capacity(0)
        .policy(FaultKind::BufferOverflow, trip_buffer_overflow())
        .build();
    let _g = node.enter(1, 1, || {}).expect("slot 1");

    let w = fake_waker(1);
    let mut cx = Context::from_waker(&w);
    let mut f1 = Box::pin(node.enter_async(2, 2, || {}));
    assert!(
        f1.as_mut().poll(&mut cx).is_pending(),
        "waiter 1 parks (capacity floored to 1)"
    );

    let mut f2 = Box::pin(node.enter_async(3, 3, || {}));
    match f2.as_mut().poll(&mut cx) {
        Poll::Ready(Err(EnterError::QueueFull)) => {}
        _ => panic!("expected QueueFull for the second waiter"),
    }

    let s = node.stats();
    assert_eq!(s.bulkhead_rejections, 1, "refusal books the shed column");
    assert_eq!(s.breaker_trips, 1, "a genuine refusal keeps the policy");
    assert_eq!(node.queue_capacity(), 0, "config value is not rewritten");
}
