//! Stage 5 (PLAN_AUDIT2 F-05, decision D2): a panicking rollback on an
//! explicit `abort()` is swallowed under `std` — same contract as `Drop`.
//! A planned abort must never escalate into a double-panic process abort.

use snubber::{ErasedGuard, SnubberEvent, SnubberGuard, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// Per-test counters: the three tests run in parallel threads of one
// binary, and a shared static would let one test's store(0) clobber
// another's increment (observed as a flaky suite failure).
fn rollback_panic(count: Arc<AtomicU64>) -> impl FnOnce() {
    move || {
        count.fetch_add(1, Ordering::SeqCst);
        panic!("rollback boom");
    }
}

#[test]
fn snubber_guard_abort_swallows_panicking_rollback() {
    let count = Arc::new(AtomicU64::new(0));
    let guard = SnubberGuard::new(3, 42, rollback_panic(Arc::clone(&count)));
    let ev = guard.abort("downstream failed");
    assert!(matches!(ev, SnubberEvent::Aborted { .. }));
    assert_eq!(count.load(Ordering::SeqCst), 1, "rollback ran once");
}

#[test]
fn tracked_guard_abort_swallows_panicking_rollback_and_releases_slot() {
    let count = Arc::new(AtomicU64::new(0));
    let node = SnubberNode::builder("abort/panic").max_in_flight(1).build();
    let guard = node
        .enter(1, 1, rollback_panic(Arc::clone(&count)))
        .expect("slot free");
    let ev = guard.abort("failed");
    assert!(matches!(ev, SnubberEvent::Aborted { .. }));
    assert_eq!(count.load(Ordering::SeqCst), 1, "rollback ran once");
    let stats = node.stats();
    assert_eq!(stats.explicit_aborts, 1, "planned abort booked");
    // The slot came home: a follow-up admission succeeds.
    let next = node.enter(2, 2, || {}).expect("slot released");
    next.commit();
}

#[test]
fn erased_guard_abort_swallows_panicking_rollback() {
    let count = Arc::new(AtomicU64::new(0));
    let guard = ErasedGuard::new(rollback_panic(Arc::clone(&count)));
    guard.abort("failed");
    assert_eq!(count.load(Ordering::SeqCst), 1, "rollback ran once");
}
