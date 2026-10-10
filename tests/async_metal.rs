//! Async metal: waker queue, stages, pacing, bounded `try_send`.
//! No runtime: 20-line `block_on` polls with a thread parker.

use snubber::{AsyncLease, FaultKind, LeaseChannel, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};

/// Minimal executor: yield until the waker fires.
fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VT)
    }
    fn wake(_: *const ()) {}
    fn wake_by_ref(_: *const ()) {}
    fn drop(_: *const ()) {}
    static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    let waker: Waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VT)) };
    let mut cx = std::task::Context::from_waker(&waker);
    let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
    loop {
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => return v,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Probe helper: poll once with a noop waker, return the verdict.
fn poll_once<F: std::future::Future + Unpin>(fut: &mut F) -> std::task::Poll<F::Output> {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VT)
    }
    fn wake(_: *const ()) {}
    fn wake_by_ref(_: *const ()) {}
    fn drop(_: *const ()) {}
    static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    let w: Waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VT)) };
    let mut cx = std::task::Context::from_waker(&w);
    std::pin::Pin::new(fut).poll(&mut cx)
}

#[test]
fn waker_queue_sleeps_and_wakes_on_commit() {
    let node = SnubberNode::builder("async/waker").max_in_flight(1).build();
    let gate = node.enter(1, 1, || {}).expect("slot 1");
    let mut fut = node.enter_async(2, 2, || {});
    assert!(poll_once(&mut fut).is_pending(), "full -> Pending");
    assert_eq!(node.waiter_len(), 1, "waker parked in the queue");
    drop(gate); // implicit drop: slot freed + waiter woken, drop counted

    // The SAME future completes cleanly on wake without burning the rollback!
    let g = block_on(fut).expect("fut completes on wake");
    g.commit();
    assert_eq!(node.stats().committed, 1, "woken slot settled");
    assert_eq!(node.stats().implicit_drops, 1, "dropped gate counted");
    assert_eq!(node.waiter_len(), 0, "queue drained");
}

#[test]
fn stage_survives_kill_and_lands_in_record() {
    let node = SnubberNode::builder("async/stage")
        .policy(
            FaultKind::ImplicitDrop,
            snubber::FaultPolicy::arm(
                snubber::FaultAction::DiscardSilently,
                snubber::LogLevel::Debug,
                "stage probe",
                "stage_probe",
            ),
        )
        .build();
    {
        let mut g = node.enter(1, 1, || {}).expect("admit");
        g.set_stage(1);
        assert_eq!(g.stage(), 1);
    }
    let s = node.stats();
    assert_eq!(s.implicit_drops, 1);
    assert_eq!(s.last_fault.expect("record kept").stage, 1);
}

#[test]
fn settle_async_commits_after_work_not_before() {
    use snubber::settle_async;
    let node = SnubberNode::builder("async/settle").build();
    let out = block_on(async {
        let g = node.enter(1, 1, || {}).expect("admit");
        settle_async(g, "db_down", async { Ok::<u32, &'static str>(7) }).await
    });
    assert_eq!(out, Ok(7));
    assert_eq!(node.stats().committed, 1);
    let out2 = block_on(async {
        let g = node.enter(2, 2, || {}).expect("admit");
        settle_async(g, "db_down", async { Err::<u32, &'static str>("x") }).await
    });
    assert_eq!(out2, Err("x"));
    assert_eq!(node.stats().explicit_aborts, 1);

    // Also test inherent method on TrackedGuard:
    let out3 = block_on(async {
        let g = node.enter(3, 3, || {}).expect("admit");
        g.settle_async(async { Ok::<u32, &'static str>(42) }).await
    });
    assert_eq!(out3, Ok(42));
    assert_eq!(node.stats().committed, 2);

    let out4 = block_on(async {
        let g = node.enter(4, 4, || {}).expect("admit");
        g.settle_async_with_reason("failed_db", async { Err::<u32, &'static str>("err") })
            .await
    });
    assert_eq!(out4, Err("err"));
    assert_eq!(node.stats().explicit_aborts, 2);
}

#[test]
fn paced_enter_uses_your_sleep_not_threads() {
    static SLEPT: AtomicU64 = AtomicU64::new(0);
    let node = SnubberNode::builder("async/pace")
        .backpressure(snubber::BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 5,
            shed_deeper_than: None,
        })
        .max_in_flight(1)
        .build();
    let _hold = node.enter(1, 1, || {}).expect("hold the only slot");
    assert!(node.friction_wait_ms() > 0, "spring loaded when full");
    let g = block_on(node.enter_paced_async(
        2,
        2,
        || {},
        |ms| async move {
            SLEPT.fetch_add(ms, Ordering::SeqCst);
        },
    ));
    assert!(g.is_err(), "full bulkhead still refuses honestly");
    assert!(SLEPT.load(Ordering::SeqCst) > 0, "sleeper ran, thread free");
}

#[test]
fn bounded_lease_try_send_counts_overflow() {
    let (tx, _rx) = LeaseChannel::pair(2);
    let mut full = 0u64;
    let mut gone = 0u64;
    let mk = |t| snubber::LeaseEvent {
        task_id: t,
        stage: 1,
        kind: FaultKind::ImplicitDrop,
    };
    assert!(LeaseChannel::fire(&tx, mk(1), &mut full, &mut gone));
    assert!(LeaseChannel::fire(&tx, mk(2), &mut full, &mut gone));
    assert!(!LeaseChannel::fire(&tx, mk(3), &mut full, &mut gone));
    assert_eq!((full, gone), (1, 0), "overflow counted, RAM fenced");
}

#[test]
fn async_lease_reaper_reports_stage_on_drop() {
    let (tx, rx) = LeaseChannel::pair(4);
    {
        let mut lease = AsyncLease::new(tx.clone(), 101);
        lease.set_stage(3);
        assert_eq!(lease.stage(), 3);
        // dropped without commit -> fires LeaseEvent to reaper
    }
    let ev = rx.try_recv().expect("reaper event delivered");
    assert_eq!(ev.task_id, 101);
    assert_eq!(ev.stage, 3);
    assert_eq!(ev.kind, FaultKind::ImplicitDrop);

    // Committed lease sends nothing to reaper:
    {
        let mut lease = AsyncLease::new(tx, 102);
        lease.set_stage(2);
        lease.commit();
    }
    assert!(rx.try_recv().is_err(), "committed lease sends nothing");
}
