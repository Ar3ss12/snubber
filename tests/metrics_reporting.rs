//! Stage 1 telemetry proofs: a fault that FIRED is always observable.
//!
//! `reported[]` is the one counter `metric_samples()` exports and the console
//! alert formats — `emit` reads `outcome.snapshot.reported[kind]`, which is
//! exactly the snapshot a `FaultHook` receives. These tests pin that
//! AUTOMATIC paths book that slot too, not only explicit `report()` calls:
//! guard settle, queue horizon, async TTL, async evict, and the breaker /
//! stale / bulkhead / rate refusals. They also pin the honest columns: an
//! explicit report and the automatic path of the same kind end on the same
//! books, and a `BufferOverflow` RECORD never contaminates the admission
//! conservation columns.

use core::task::{Context, Poll, Waker};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::Wake;

use snubber::{
    EnterError, FaultKind, FaultRecord, OverflowPolicy, SnubberNode, SnubberStats,
    TransactionHeader,
};

/// Distinct-allocation waker — safe (no `unsafe`), strict-provenance clean.
fn fake_waker(id: usize) -> Waker {
    struct FakeWake(usize);
    impl Wake for FakeWake {
        fn wake(self: Arc<Self>) {
            let _ = self.0;
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let _ = self.0;
        }
    }
    Waker::from(Arc::new(FakeWake(id)))
}

/// Sum of exported counts for one kind (0 when the metric row is absent).
fn export_count(node: &SnubberNode, kind: FaultKind) -> u64 {
    node.metric_samples()
        .iter()
        .filter(|r| r.kind == kind.index())
        .map(|r| r.count)
        .sum()
}

#[test]
fn guard_implicit_drop_is_exported_and_alerted() {
    let seen: Arc<Mutex<Vec<(usize, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let node = SnubberNode::builder("exp/guard")
        .on_fault(move |rec: &FaultRecord, stats: &SnubberStats, _| {
            sink.lock()
                .unwrap()
                .push((rec.kind.index(), stats.reported[rec.kind.index()]));
        })
        .build();

    let g = node.enter(1, 1, || {}).expect("admit");
    drop(g); // automatic guard settle

    let s = node.stats();
    assert_eq!(s.implicit_drops, 1);
    assert_eq!(
        s.reported[FaultKind::ImplicitDrop.index()],
        1,
        "the guard settle must book the reported slot"
    );

    let rows = node.metric_samples();
    let row = rows
        .iter()
        .find(|r| r.kind == FaultKind::ImplicitDrop.index())
        .expect("implicit_drops_total must be exported");
    assert_eq!(row.tag, "implicit_drops_total");
    assert_eq!(row.count, 1);

    // The hook received the SAME snapshot `emit` formats into the alert
    // (`outcome.snapshot.reported[kind]`) — a non-zero count here proves the
    // alert line is not `count=0`.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "exactly one fault fired");
    assert_eq!(seen[0], (FaultKind::ImplicitDrop.index(), 1));
}

#[test]
fn async_ttl_timeout_is_exported() {
    let node = SnubberNode::builder("exp/ttl")
        .max_in_flight(1)
        .queue_capacity(4)
        .task_ttl_ms(10)
        .build();
    let held = node.enter(1, 1, || {}).expect("hold the slot");

    let w = fake_waker(1);
    let mut cx = Context::from_waker(&w);
    let mut fut = Box::pin(node.enter_async(2, 2, || {}));
    assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

    std::thread::sleep(std::time::Duration::from_millis(25));
    held.commit();

    assert!(matches!(
        fut.as_mut().poll(&mut cx),
        Poll::Ready(Err(EnterError::QueueTtlExpired))
    ));

    let s = node.stats();
    assert_eq!(
        s.reported[FaultKind::Timeout.index()],
        1,
        "async TTL must export Timeout (the watchdog path used to be the only one)"
    );
    let rows = node.metric_samples();
    let row = rows
        .iter()
        .find(|r| r.kind == FaultKind::Timeout.index())
        .expect("watchdog_timeouts_total must be exported");
    assert_eq!(row.tag, "watchdog_timeouts_total");
    assert_eq!(export_count(&node, FaultKind::Timeout), 1);
}

#[test]
fn async_evict_oldest_is_exported() {
    let node = SnubberNode::builder("exp/evict")
        .max_in_flight(1)
        .queue_capacity(2)
        .shed_policy(OverflowPolicy::EvictOldest)
        .build();
    let held = node.enter(1, 1, || {}).expect("hold the slot");

    let w1 = fake_waker(1);
    let mut cx1 = Context::from_waker(&w1);
    let mut f1 = Box::pin(node.enter_async(2, 2, || {}));
    assert!(matches!(f1.as_mut().poll(&mut cx1), Poll::Pending));

    let w2 = fake_waker(2);
    let mut cx2 = Context::from_waker(&w2);
    let mut f2 = Box::pin(node.enter_async(3, 3, || {}));
    assert!(matches!(f2.as_mut().poll(&mut cx2), Poll::Pending));

    // Third waiter overflows the cap: the oldest (f1) is evicted.
    let w3 = fake_waker(3);
    let mut cx3 = Context::from_waker(&w3);
    let mut f3 = Box::pin(node.enter_async(4, 4, || {}));
    assert!(matches!(f3.as_mut().poll(&mut cx3), Poll::Pending));

    let s = node.stats();
    assert_eq!(s.evicted, 1);
    assert_eq!(
        s.reported[FaultKind::EvictedOldest.index()],
        1,
        "F-10: the async eviction must book the reported slot too"
    );
    assert_eq!(export_count(&node, FaultKind::EvictedOldest), 1);
    drop(held);
}

#[test]
fn automatic_refusals_are_exported() {
    // BulkheadFull — a held slot refuses the newcomer.
    let bulk = SnubberNode::builder("exp/bulk").max_in_flight(1).build();
    let _h = bulk.enter(1, 1, || {}).expect("hold");
    assert!(matches!(
        bulk.enter(2, 2, || {}),
        Err(EnterError::BulkheadFull)
    ));
    let s = bulk.stats();
    assert_eq!(s.reported[FaultKind::BulkheadFull.index()], 1);
    assert_eq!(s.bulkhead_rejections, 1);
    assert_eq!(export_count(&bulk, FaultKind::BulkheadFull), 1);

    // StaleEpoch — a header behind the gate is cut.
    let stale = SnubberNode::builder("exp/stale").build();
    assert!(stale.observe(&TransactionHeader::new(10, 10)));
    assert!(!stale.observe(&TransactionHeader::new(9, 9)));
    let s = stale.stats();
    assert_eq!(s.reported[FaultKind::StaleEpoch.index()], 1);
    assert_eq!(s.stale_epochs_rejected, 1);
    assert_eq!(export_count(&stale, FaultKind::StaleEpoch), 1);

    // RateLimited — the token bucket is empty.
    let rate = SnubberNode::builder("exp/rate").rate_per_sec(1).build();
    rate.enter(1, 1, || {})
        .expect("first consumes the token")
        .commit();
    assert!(matches!(
        rate.enter(2, 2, || {}),
        Err(EnterError::RateLimited)
    ));
    let s = rate.stats();
    assert_eq!(s.reported[FaultKind::RateLimited.index()], 1);
    assert_eq!(s.rate_rejected, 1);
    assert_eq!(export_count(&rate, FaultKind::RateLimited), 1);

    // BreakerOpen — trip, fail the probe, then fast-reject.
    let brk = SnubberNode::builder("exp/breaker")
        .trip_after_failures(1)
        .build();
    brk.report(FaultKind::Panic, 1, 1); // Closed -> HalfOpen
    {
        let g = brk.enter(2, 2, || {}).expect("probe admitted");
        drop(g); // probe implicit drop -> Open
    }
    assert!(matches!(
        brk.enter(3, 3, || {}),
        Err(EnterError::BreakerOpen)
    ));
    let s = brk.stats();
    assert_eq!(s.reported[FaultKind::BreakerOpen.index()], 1);
    assert_eq!(s.breaker_rejections, 1);
    assert_eq!(export_count(&brk, FaultKind::BreakerOpen), 1);
}

#[test]
fn buffer_overflow_record_never_books_a_conservation_column() {
    // F-09: a BufferOverflow RECORD is capacity telemetry, never a refusal.
    // Named by `report()` or raised by the automatic queue horizon, it must
    // NOT enter `bulkhead_rejections` / `shed_bulkhead()` / `shed_total()`:
    // an admission above the horizon still books `total_started`, so a shed
    // bump there would double-count one attempt.

    // Explicit report.
    let named = SnubberNode::builder("overflow/named").build();
    for i in 1..=5u64 {
        named.report(FaultKind::BufferOverflow, i, i);
    }
    let s = named.stats();
    assert_eq!(s.reported[FaultKind::BufferOverflow.index()], 5);
    assert_eq!(
        s.bulkhead_rejections, 0,
        "explicit report must not contaminate bulkhead"
    );
    assert_eq!(s.shed_bulkhead(), 0);
    assert_eq!(s.shed_total(), 0);
    assert_eq!(export_count(&named, FaultKind::BufferOverflow), 5);

    // Automatic horizon: `queue_capacity(0)` makes `queued >= queue_limit`
    // always true, yet the admission still SUCCEEDS.
    let horizon = SnubberNode::builder("overflow/horizon")
        .queue_capacity(0)
        .build();
    let g = horizon.enter(1, 1, || {}).expect("admit above the horizon");
    g.commit();
    let s = horizon.stats();
    assert_eq!(s.total_started, 1);
    assert_eq!(s.committed, 1);
    assert_eq!(
        s.reported[FaultKind::BufferOverflow.index()],
        1,
        "the horizon record exports"
    );
    assert_eq!(s.bulkhead_rejections, 0, "the horizon is telemetry-only");
    assert_eq!(export_count(&horizon, FaultKind::BufferOverflow), 1);
    // Full admission law still holds: one attempt, one term.
    assert_eq!(
        s.admission_total(),
        s.total_started.saturating_add(s.shed_total()),
        "admission_total == total_started + shed_total"
    );
}

#[test]
fn explicit_report_books_the_same_column_as_the_automatic_path() {
    // F-01/F-09 parity: one explicit `report(kind)` leaves the SAME books a
    // single automatic event of that kind leaves.

    // BulkheadFull: automatic refusal vs explicit report.
    let auto = SnubberNode::builder("parity/bulk_auto")
        .max_in_flight(1)
        .build();
    let _h = auto.enter(1, 1, || {}).expect("hold");
    assert!(matches!(
        auto.enter(2, 2, || {}),
        Err(EnterError::BulkheadFull)
    ));
    let named = SnubberNode::builder("parity/bulk_named").build();
    named.report(FaultKind::BulkheadFull, 1, 1);
    let (a, n) = (auto.stats(), named.stats());
    assert_eq!(a.bulkhead_rejections, 1);
    assert_eq!(a.bulkhead_rejections, n.bulkhead_rejections, "same column");
    assert_eq!(
        a.reported[FaultKind::BulkheadFull.index()],
        n.reported[FaultKind::BulkheadFull.index()],
        "same reported slot"
    );

    // PoisonedPayload: admission-boundary refusal vs explicit report (the
    // column `bump_column` used to skip, so the two books disagreed).
    let boundary = SnubberNode::builder("parity/poison_auto").build();
    assert!(matches!(
        boundary.enter_checked(
            1,
            1,
            &0u64,
            |p| if *p == 0 { Err("bad") } else { Ok(()) },
            || {}
        ),
        Err(EnterError::InvariantBreach(_))
    ));
    let named = SnubberNode::builder("parity/poison_named").build();
    named.report(FaultKind::PoisonedPayload, 1, 1);
    let (a, n) = (boundary.stats(), named.stats());
    assert_eq!(a.poisoned_rejected, 1);
    assert_eq!(a.poisoned_rejected, n.poisoned_rejected, "same column");
    assert_eq!(
        a.reported[FaultKind::PoisonedPayload.index()],
        n.reported[FaultKind::PoisonedPayload.index()],
        "same reported slot"
    );

    // BufferOverflow: NEITHER path books a column (telemetry only) — the
    // regression F-09 guarded against.
    let named = SnubberNode::builder("parity/overflow_named").build();
    named.report(FaultKind::BufferOverflow, 1, 1);
    assert_eq!(named.stats().bulkhead_rejections, 0);
}
