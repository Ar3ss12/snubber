use snubber::{AsyncLease, EnterError, FaultKind, LeaseChannel, OverflowPolicy, SnubberNode};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};

static WOKEN: AtomicBool = AtomicBool::new(false);

fn create_tracked_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VT)
    }
    unsafe fn wake(_: *const ()) {
        WOKEN.store(true, Ordering::SeqCst);
    }
    unsafe fn wake_by_ref(_: *const ()) {
        WOKEN.store(true, Ordering::SeqCst);
    }
    unsafe fn drop(_: *const ()) {}

    static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VT)) }
}

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

#[test]
fn waker_queue_sleeps_and_wakes_on_commit() {
    WOKEN.store(false, Ordering::SeqCst);
    let node = SnubberNode::builder("async/waker").max_in_flight(1).build();
    let gate = node.enter(1, 1, || {}).expect("slot 1");

    let mut fut = node.enter_async(2, 2, || {});

    // Poll with our tracked waker:
    let waker = create_tracked_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(core::pin::Pin::new(&mut fut).poll(&mut cx).is_pending());
    assert!(
        !WOKEN.load(Ordering::SeqCst),
        "must not be woken while slot held"
    );
    assert_eq!(node.waiter_len(), 1, "waker parked in the queue");

    // Release slot: node MUST explicitly invoke waker.wake()
    drop(gate);
    assert!(
        WOKEN.load(Ordering::SeqCst),
        "node MUST explicitly invoke waker.wake() on slot release"
    );

    // Now the future claims the slot on next poll:
    match core::pin::Pin::new(&mut fut).poll(&mut cx) {
        std::task::Poll::Ready(Ok(guard)) => {
            guard.commit();
        }
        std::task::Poll::Ready(Err(e)) => panic!("expected Ready(Ok(guard)), got Err({e:?})"),
        std::task::Poll::Pending => panic!("expected Ready(Ok(guard)), got Pending"),
    }
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
        g.checkpoint(2, "headers_sent");
        assert_eq!(g.stage(), 2);
    }
    let s = node.stats();
    assert_eq!(s.implicit_drops, 1);
    let fault = s.last_fault.expect("record kept");
    assert_eq!(fault.stage, 2);
    assert_eq!(fault.stage_label, Some("headers_sent"));
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

#[test]
fn enter_async_repeated_polls_do_not_bloat_waiter_queue() {
    let node = SnubberNode::builder("test_waker_dedup")
        .max_in_flight(1)
        .queue_limit(4)
        .build();

    // Hold the single available slot.
    let _g = node.enter_auto(|| {}).expect("admit slot");

    // Create an async request.
    let mut fut = node.enter_async_auto(|| {});

    // Emulate 20 repeated poll calls for the exact same task waker.
    fn clone(_: *const ()) -> std::task::RawWaker {
        std::task::RawWaker::new(std::ptr::null(), &VT)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    static VT: std::task::RawWakerVTable =
        std::task::RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    let waker: Waker = unsafe { Waker::from_raw(std::task::RawWaker::new(std::ptr::null(), &VT)) };
    let mut cx = std::task::Context::from_waker(&waker);

    for _ in 0..20 {
        let res = core::pin::Pin::new(&mut fut).poll(&mut cx);
        assert!(res.is_pending(), "must stay pending while slot is held");
    }

    // Due to will_wake deduplication, exactly 1 waker remains in the queue.
    assert_eq!(
        node.waiter_len(),
        1,
        "waiters queue must not bloat on repeated polls"
    );
}

// --- Regression: EvictOldest waker loss + dropped-future deregistration ---

static WOKEN_A: AtomicBool = AtomicBool::new(false);
static WOKEN_B: AtomicBool = AtomicBool::new(false);

fn waker_a() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VT_A)
    }
    unsafe fn wake(_: *const ()) {
        WOKEN_A.store(true, Ordering::SeqCst);
    }
    unsafe fn wake_by_ref(_: *const ()) {
        WOKEN_A.store(true, Ordering::SeqCst);
    }
    unsafe fn drop(_: *const ()) {}
    static VT_A: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VT_A)) }
}

fn waker_b() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VT_B)
    }
    unsafe fn wake(_: *const ()) {
        WOKEN_B.store(true, Ordering::SeqCst);
    }
    unsafe fn wake_by_ref(_: *const ()) {
        WOKEN_B.store(true, Ordering::SeqCst);
    }
    unsafe fn drop(_: *const ()) {}
    static VT_B: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VT_B)) }
}

#[test]
fn evict_oldest_wakes_the_evicted_future_with_queue_full() {
    // Regression: EvictOldest used to `pop_front` the oldest parked waker
    // WITHOUT waking it — that task stayed Pending forever (zombie), and
    // re-polling it re-parked, evicting another victim: a wake/evict
    // livelock. Now the evicted future is marked and WOKEN, and its next
    // poll resolves to an honest QueueFull.
    WOKEN_A.store(false, Ordering::SeqCst);
    WOKEN_B.store(false, Ordering::SeqCst);
    let node = SnubberNode::builder("async/evict")
        .max_in_flight(1)
        .queue_limit(1)
        .overflow(OverflowPolicy::EvictOldest)
        .build();
    let _slot = node.enter(1, 1, || {}).expect("occupy the only slot");

    let mut fut_a = node.enter_async(2, 2, || {});
    let wa = waker_a();
    let cx_a = &mut std::task::Context::from_waker(&wa);
    assert!(
        core::pin::Pin::new(&mut fut_a).poll(cx_a).is_pending(),
        "future A parks behind the slot"
    );

    let mut fut_b = node.enter_async(3, 3, || {});
    let wb = waker_b();
    let cx_b = &mut std::task::Context::from_waker(&wb);
    assert!(
        core::pin::Pin::new(&mut fut_b).poll(cx_b).is_pending(),
        "future B parks in the evicted slot"
    );
    assert!(
        WOKEN_A.load(Ordering::SeqCst),
        "eviction MUST wake the evicted task, not drop its waker"
    );
    match core::pin::Pin::new(&mut fut_a).poll(cx_a) {
        std::task::Poll::Ready(Err(EnterError::QueueFull)) => {}
        std::task::Poll::Ready(Ok(_)) => panic!("evicted future must NOT be admitted"),
        std::task::Poll::Pending => panic!("evicted future must not hang pending"),
        std::task::Poll::Ready(Err(_)) => panic!("evicted future must be QueueFull"),
    }
}

#[test]
fn dropped_parked_future_deregisters_its_queue_slot() {
    // Regression: a future dropped while parked left its waker in the
    // queue forever — a slot nobody will ever wake, starving later
    // admissions (head-of-line blocking by a dead task).
    let node = SnubberNode::builder("async/drop_waiter")
        .max_in_flight(1)
        .build();
    let _slot = node.enter(1, 1, || {}).expect("occupy the only slot");

    let mut fut = node.enter_async(2, 2, || {});
    let wa = waker_a();
    let cx = &mut std::task::Context::from_waker(&wa);
    assert!(core::pin::Pin::new(&mut fut).poll(cx).is_pending());
    assert_eq!(node.waiter_len(), 1, "parked before drop");

    drop(fut);
    assert_eq!(
        node.waiter_len(),
        0,
        "dropped future must deregister its own queue entry"
    );

    // The freed queue admits a new waiter and wakes it on release.
    let mut fut2 = node.enter_async(3, 3, || {});
    assert!(core::pin::Pin::new(&mut fut2).poll(cx).is_pending());
    assert_eq!(node.waiter_len(), 1);
    drop(_slot);
    assert!(WOKEN_A.load(Ordering::SeqCst), "real waiter must be woken");
}

#[test]
fn admitted_on_spurious_repoll_leaves_no_ghost_waiter() {
    use std::task::{Context, Poll};

    let node = SnubberNode::builder("async/ghost")
        .max_in_flight(1)
        .queue_limit(2)
        .overflow(OverflowPolicy::RejectNew)
        .build();
    let waker = create_tracked_waker();
    let mut cx = Context::from_waker(&waker);

    let held = node.enter(1, 1, || {}).expect("slot");

    let mut f1 = Box::pin(node.enter_async(2, 2, || {}));
    assert!(f1.as_mut().poll(&mut cx).is_pending(), "f1 parks");
    let mut f2 = Box::pin(node.enter_async(3, 3, || {}));
    assert!(f2.as_mut().poll(&mut cx).is_pending(), "f2 parks");

    // Settles the held guard: pops the front waiter (f1) and frees the slot.
    held.abort("ghost test abort");

    // Spec-legal spurious re-poll of f2: it is admitted while its own
    // queue entry is still parked.
    let guard = match f2.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(g)) => g,
        _ => panic!("f2 must be admitted on the re-poll"),
    };
    // The future object dies here while the guard lives on. A Drop gated
    // on "currently parked" used to leave f2's entry as a ghost that
    // consumed queue capacity forever.
    drop(f2);

    // Two more parkers: with the ghost present the second one was falsely
    // rejected with QueueFull although no real waiter occupied the queue.
    let mut f3 = Box::pin(node.enter_async(4, 4, || {}));
    assert!(f3.as_mut().poll(&mut cx).is_pending());
    let mut f4 = Box::pin(node.enter_async(5, 5, || {}));
    match f4.as_mut().poll(&mut cx) {
        Poll::Pending => {}
        Poll::Ready(Ok(_)) => panic!("f4 must park, not be admitted"),
        Poll::Ready(Err(e)) => panic!("ghost waiter consumed queue capacity: {e:?}"),
    }

    guard.commit();
}
