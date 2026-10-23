//! Transition state machine + orphan contract pins.
use snubber::{NoOrphanGuard, OrphanGuard, SnubberNode};
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clicker {
    Idle,
    Running,
}
#[test]
fn transition_moves_one_enum_atomically() {
    let node = SnubberNode::builder("sm/one").build();
    let mut state = Clicker::Idle;
    node.transition(
        &mut state,
        Clicker::Running,
        || {},
        |s| {
            assert_eq!(*s, Clicker::Idle);
            Ok::<(), &'static str>(())
        },
    )
    .expect("move");
    assert_eq!(state, Clicker::Running);
}
#[test]
fn transition_failure_keeps_old_state() {
    let node = SnubberNode::builder("sm/old").build();
    let mut state = Clicker::Idle;
    let r = node.transition(
        &mut state,
        Clicker::Running,
        || {},
        |_| Err::<(), &'static str>("jammed"),
    );
    assert!(r.is_err());
    assert_eq!(state, Clicker::Idle, "old state restored");
    let s = node.stats();
    assert_eq!(s.committed, 0);
    assert_eq!(s.explicit_aborts + s.implicit_drops, 1);
}
#[test]
fn orphan_contract_is_typed_and_honest() {
    let g = NoOrphanGuard;
    assert!(!g.arm(1), "no-op promises nothing");
    assert!(g.backend_name().contains("no-op"));
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<NoOrphanGuard>();
}
#[test]
fn preempt_storm_on_critical_flood() {
    use snubber::{find_anomalies, AnomalyKind, AnomalyPolicy, EnterError, Priority, SnubberNode};
    let node = SnubberNode::builder("preempt/storm").build();
    // 200 admissions against a default breaker (trip_after_failures = 3):
    // the first guards arm and drop (sickness), the breaker opens, one
    // HalfOpen probe arms, the rest are BreakerOpen refusals. The old
    // swallowed `let _ =` shape hid all of that; every outcome is now
    // counted and tied to the node's own books.
    let mut started = 0u64;
    let mut refused = 0u64;
    for i in 1..=200u64 {
        match node.enter_with_priority(Priority::Normal, i, i, || {}) {
            Ok(g) => {
                drop(g);
                started += 1;
            }
            Err(EnterError::BreakerOpen) => refused += 1,
            Err(other) => panic!("unexpected refusal: {other:?}"),
        }
    }
    // Simulate: 150 of them filed preempt requests.
    // (Direct: report Preempted is counted; admission math via stats.)
    for _ in 0..150 {
        node.report(snubber::FaultKind::Preempted, 1, 1);
    }
    let hits = find_anomalies("preempt/storm", &node.stats(), &AnomalyPolicy::default());
    assert!(
        hits.iter().any(|h| h.kind == AnomalyKind::PreemptStorm),
        "critical preemption flood must trigger anomaly verdict, got {hits:?}"
    );
    // Conservation: every attempt lands in exactly one bucket, local
    // counts agree with the node, and every dropped guard is honestly
    // booked as an implicit drop.
    let s = node.stats();
    assert_eq!(started + refused, 200, "no attempt vanished");
    assert_eq!(started, s.total_started, "armed guards match the node");
    assert_eq!(refused, s.breaker_rejections, "refusals match the node");
    assert_eq!(
        s.implicit_drops, started,
        "each dropped guard is an implicit drop"
    );
    assert_eq!(s.committed, 0, "nothing committed in the flood");
    assert!(started >= 1, "the flood must arm at least once");
}
