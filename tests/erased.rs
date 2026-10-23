//! Layer 1/3 (alloc): Type-erased guards and capturing closure policies.
//!
//! Merged test suite covering:
//! - [`ErasedGuard`] struct-field embedding, context capture, and commit hooks
//! - [`ClosurePolicy`] capturing self-healing commands and debug representation
//! - [`FaultDetail`] zero-cost static strings and owned formatting
//! - `BlockCaller` backpressure behavior

#![cfg(feature = "alloc")]

use snubber::{
    ClosurePolicy, EnterError, ErasedGuard, FaultAction, FaultDetail, FaultKind, FaultPolicy,
    LogLevel, SnubberNode,
};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

// The struct-field shape that was impossible with TrackedGuard<F>:
// every closure is a different anonymous type; ErasedGuard erases once.
struct App {
    hold: Option<ErasedGuard>,
}

#[test]
fn erased_guard_lives_in_struct_field() {
    static RELEASES: AtomicU64 = AtomicU64::new(0);
    let node = SnubberNode::builder("erased/field")
        .max_in_flight(1)
        .build();
    let mut app = App { hold: None };
    app.hold = Some(
        node.enter_erased(1, 7, || {
            RELEASES.fetch_add(1, Ordering::SeqCst);
        })
        .expect("slot free"),
    );
    // Thread death: field dropped without commit → rollback fires.
    drop(app);
    assert_eq!(RELEASES.load(Ordering::SeqCst), 1, "rollback exactly once");
}

#[test]
fn erased_guard_commit_disarms() {
    static RELEASES: AtomicU64 = AtomicU64::new(0);
    let before = RELEASES.load(Ordering::SeqCst);
    let g = ErasedGuard::new(|| {
        RELEASES.fetch_add(1, Ordering::SeqCst);
    });
    g.commit();
    assert_eq!(RELEASES.load(Ordering::SeqCst), before, "commit disarms");
}

#[test]
fn erased_guard_captures_context() {
    let fired = Arc::new(AtomicU64::new(0));
    let c = fired.clone();
    let node = SnubberNode::builder("erased/capture").build();
    let g = node
        .enter_erased(1, 1, move || {
            c.fetch_add(41, Ordering::SeqCst);
        })
        .expect("admit");
    drop(g); // no commit → rollback with captures
    assert_eq!(fired.load(Ordering::SeqCst), 41);
}

#[test]
fn erased_guard_rejection_boxes_nothing() {
    let node = SnubberNode::builder("erased/full")
        .max_in_flight(0) // always full
        .build();
    let r = node.enter_erased(1, 1, || panic!("must never box"));
    assert!(r.is_err(), "rejection before boxing");
}

#[test]
fn fault_detail_static_is_zero_cost() {
    let d = FaultDetail::Static("cps == 0");
    assert!(d.is_static());
    assert_eq!(d.detail(), "cps == 0");
    assert_eq!(d.to_string(), "cps == 0");
    let from: FaultDetail = "interval == 0".into();
    assert_eq!(from.detail(), "interval == 0");
}

#[test]
fn fault_detail_owned_carries_live_values() {
    let d = FaultDetail::from(format!("interval={}", 359_999_640_000u64));
    assert!(!d.is_static());
    assert_eq!(d.detail(), "interval=359999640000");
}

#[test]
fn block_caller_parks_then_admits_freed_slot() {
    let node = SnubberNode::builder("block/admit")
        .max_in_flight(1)
        .policy(
            FaultKind::BulkheadFull,
            FaultPolicy::arm(
                FaultAction::BlockCaller(50),
                LogLevel::Debug,
                "parked for slot",
                "block_parks_total",
            ),
        )
        .build();
    let probe = node.enter(1, 1, || {}).expect("first slot");
    let n2 = node.clone();
    let parked = std::thread::spawn(move || n2.enter(2, 2, || {}));
    std::thread::sleep(std::time::Duration::from_millis(10));
    probe.commit(); // slot frees while the caller is parked
    let guard = parked.join().expect("thread").expect("admitted after park");
    guard.commit();
    assert_eq!(node.stats().committed, 2);
}

#[test]
fn block_caller_rejects_when_slot_never_frees() {
    let node = SnubberNode::builder("block/reject")
        .max_in_flight(1)
        .policy(
            FaultKind::BulkheadFull,
            FaultPolicy::arm(
                FaultAction::BlockCaller(20),
                LogLevel::Debug,
                "parked for slot",
                "block_parks_total",
            ),
        )
        .build();
    let _held = node.enter(1, 1, || {}).expect("first slot");
    let start = std::time::Instant::now();
    let r = node.enter(2, 2, || {});
    let parked_ms = start.elapsed().as_millis();
    assert!(
        matches!(r, Err(EnterError::BulkheadFull)),
        "reject, not hang"
    );
    assert!(parked_ms >= 10, "actually parked: {parked_ms}ms");
    assert_eq!(node.stats().bulkhead_rejections, 1);
}

#[test]
fn erased_guard_with_commit_hook_fires_on_commit() {
    static COMMITS: AtomicU64 = AtomicU64::new(0);
    static ROLLBACKS: AtomicU64 = AtomicU64::new(0);

    let g = ErasedGuard::with_commit_hook(
        || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        },
        || {
            COMMITS.fetch_add(1, Ordering::SeqCst);
        },
    );
    g.commit();

    assert_eq!(COMMITS.load(Ordering::SeqCst), 1, "commit hook fired");
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 0, "rollback disarmed");
}

#[test]
fn node_enter_auto_mints_monotonic_sequences() {
    let node = SnubberNode::builder("auto/seq").max_in_flight(10).build();
    let g1 = node.enter_auto(|| {}).expect("admit 1");
    let g2 = node.enter_auto(|| {}).expect("admit 2");
    assert!(g2.header().epoch > g1.header().epoch);
    g1.commit();
    g2.commit();
}

#[test]
fn closure_captures_context() {
    let attempts = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&attempts);
    let policy = ClosurePolicy::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    });
    policy.run();
    policy.run();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn closure_debug_does_not_leak_captures() {
    let x = 42u32;
    let policy = ClosurePolicy::new(move || {
        let _ = x;
    });
    assert_eq!(format!("{:?}", policy), "ClosurePolicy(..)");
}

// Regression (0.6.2, bug 8): the admission-time throwaway probe commit
// used to close a HalfOpen breaker while the real transaction's outcome
// was still unknown. The probe must settle with the ERASED guard.
#[test]
fn erased_admission_does_not_close_half_open_breaker() {
    let node = SnubberNode::builder("erased/probe")
        .trip_after_failures(1)
        .build();
    {
        let _g = node.enter(1, 1, || {}).expect("enter");
    } // implicit drop -> trip -> HalfOpen
    assert_eq!(node.stats().breaker_state, snubber::BreakerState::HalfOpen);

    let g = node.enter_erased(2, 2, || {}).expect("probe admitted");
    // The throwaway commit must NOT have closed the breaker: a second
    // call is refused while the erased guard holds the probe.
    assert!(
        matches!(node.enter(3, 3, || {}), Err(EnterError::BreakerOpen)),
        "probe must stay armed, not consumed by the erased admission"
    );

    g.commit();
    assert_eq!(
        node.stats().breaker_state,
        snubber::BreakerState::Closed,
        "erased commit settles the probe: breaker closes"
    );
    node.enter(4, 4, || {}).expect("admits after recovery").commit();
}

#[test]
fn erased_drop_reopens_breaker_like_a_failed_probe() {
    let node = SnubberNode::builder("erased/probe-drop")
        .trip_after_failures(1)
        .build();
    {
        let _g = node.enter(1, 1, || {}).expect("enter");
    } // HalfOpen
    let g = node.enter_erased(2, 2, || {}).expect("probe admitted");
    drop(g); // rollback without commit: downstream still degraded
    assert_eq!(
        node.stats().breaker_state,
        snubber::BreakerState::Open,
        "failed erased probe must re-open the breaker"
    );
    assert!(matches!(node.enter(3, 3, || {}), Err(EnterError::BreakerOpen)));
    node.reset_breaker();
    node.enter(4, 4, || {}).expect("recovers").commit();
}
