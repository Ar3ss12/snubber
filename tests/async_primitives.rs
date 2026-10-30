use snubber::{
    AsyncLease, BreakerState, EnterError, FaultKind, LeaseChannel, OverflowPolicy, SnubberNode,
};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};

static WOKEN: AtomicBool = AtomicBool::new(false);
static EVICT_HOOK_HITS: AtomicU64 = AtomicU64::new(0);

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
    // Private wake flag: the shared `WOKEN` static is written by other
    // tests' wakers running in parallel threads, which made the negative
    // assert below race (spurious PASS/FAIL depending on scheduling).
    static WOKEN_WQ: AtomicBool = AtomicBool::new(false);
    fn waker_wq() -> Waker {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VT_WQ)
        }
        unsafe fn wake(_: *const ()) {
            WOKEN_WQ.store(true, Ordering::SeqCst);
        }
        unsafe fn wake_by_ref(_: *const ()) {
            WOKEN_WQ.store(true, Ordering::SeqCst);
        }
        unsafe fn drop(_: *const ()) {}
        static VT_WQ: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VT_WQ)) }
    }

    WOKEN_WQ.store(false, Ordering::SeqCst);
    let node = SnubberNode::builder("async/waker").max_in_flight(1).build();
    let gate = node.enter(1, 1, || {}).expect("slot 1");

    let mut fut = node.enter_async(2, 2, || {});

    // Poll with our tracked waker:
    let waker = waker_wq();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(core::pin::Pin::new(&mut fut).poll(&mut cx).is_pending());
    assert!(
        !WOKEN_WQ.load(Ordering::SeqCst),
        "must not be woken while slot held"
    );
    assert_eq!(node.waiter_len(), 1, "waker parked in the queue");

    // Release slot: node MUST explicitly invoke waker.wake()
    drop(gate);
    assert!(
        WOKEN_WQ.load(Ordering::SeqCst),
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
fn re_park_replaces_waker_once_and_wakes_only_the_new_one() {
    // Wakers record into caller-owned flags: a re-poll of a parked future
    // must overwrite its queue entry (not append a second one), and only
    // the newest waker may fire when the slot frees.
    static OLD_WOKEN: AtomicBool = AtomicBool::new(false);
    static NEW_WOKEN: AtomicBool = AtomicBool::new(false);
    OLD_WOKEN.store(false, Ordering::SeqCst);
    NEW_WOKEN.store(false, Ordering::SeqCst);
    let node = SnubberNode::builder("async/re-park")
        .max_in_flight(1)
        .build();
    let gate = node.enter(1, 1, || {}).expect("slot 1");
    let mut fut = node.enter_async(2, 2, || {});

    let old = flag_waker(&OLD_WOKEN);
    let mut old_cx = std::task::Context::from_waker(&old);
    assert!(core::pin::Pin::new(&mut fut).poll(&mut old_cx).is_pending());
    assert_eq!(node.waiter_len(), 1);

    let new = flag_waker(&NEW_WOKEN);
    let mut new_cx = std::task::Context::from_waker(&new);
    assert!(core::pin::Pin::new(&mut fut).poll(&mut new_cx).is_pending());
    assert_eq!(
        node.waiter_len(),
        1,
        "a re-poll updates the entry instead of enqueueing a second waker"
    );

    drop(gate);
    assert!(
        NEW_WOKEN.load(Ordering::SeqCst),
        "the replacement waker must be the one woken"
    );
    assert!(
        !OLD_WOKEN.load(Ordering::SeqCst),
        "the replaced waker must not be left in the queue"
    );
    match core::pin::Pin::new(&mut fut).poll(&mut new_cx) {
        std::task::Poll::Ready(Ok(guard)) => {
            guard.commit();
        }
        std::task::Poll::Ready(Err(error)) => {
            panic!("expected Ready(Ok(_)), got Err({error:?})")
        }
        std::task::Poll::Pending => panic!("expected Ready(Ok(_)), got Pending"),
    }
    assert_eq!(node.waiter_len(), 0);
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

    // Identity dedup (an `Arc::ptr_eq` match on the future's own `evicted`
    // flag) updates this entry in place, so exactly 1 waker remains in the
    // queue. A `will_wake` scan is NOT the mechanism: two futures polled by
    // one task share a waker and would cross-match.
    assert_eq!(
        node.waiter_len(),
        1,
        "waiters queue must not bloat on repeated polls"
    );
}

// --- Regression: EvictOldest waker loss + dropped-future deregistration ---

/// A waker that records into a CALLER-OWNED flag.
///
/// Each test below declares its own `static` flags and passes them here.
/// The previous shape (one shared `WOKEN_A`/`WOKEN_B` written by every
/// waker in the file) let a parallel test reset or set the bit another
/// test was asserting — the same defect that `waker_queue_sleeps_and_wakes_on_commit`
/// documents for `WOKEN`, and it poisoned both directions: a positive
/// assert could pass on another test's wake, a negative one could fail on
/// another test's reset.
fn flag_waker(flag: &'static AtomicBool) -> Waker {
    unsafe fn clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &VT_FLAG)
    }
    unsafe fn wake(data: *const ()) {
        (*(data as *const AtomicBool)).store(true, Ordering::SeqCst);
    }
    unsafe fn wake_by_ref(data: *const ()) {
        (*(data as *const AtomicBool)).store(true, Ordering::SeqCst);
    }
    unsafe fn drop(_: *const ()) {}
    static VT_FLAG: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe {
        Waker::from_raw(RawWaker::new(
            flag as *const AtomicBool as *const (),
            &VT_FLAG,
        ))
    }
}

#[test]
fn evict_oldest_wakes_the_evicted_future_with_queue_full() {
    static EVICTED_A: AtomicBool = AtomicBool::new(false);
    static EVICTED_B: AtomicBool = AtomicBool::new(false);

    // Regression: EvictOldest used to `pop_front` the oldest parked waker
    // WITHOUT waking it — that task stayed Pending forever (zombie), and
    // re-polling it re-parked, evicting another victim: a wake/evict
    // livelock. Now the evicted future is marked and WOKEN, and its next
    // poll resolves to an honest QueueFull.
    EVICTED_A.store(false, Ordering::SeqCst);
    EVICTED_B.store(false, Ordering::SeqCst);
    let node = SnubberNode::builder("async/evict")
        .max_in_flight(1)
        .queue_limit(1)
        .overflow(OverflowPolicy::EvictOldest)
        .build();
    let _slot = node.enter(1, 1, || {}).expect("occupy the only slot");

    let mut fut_a = node.enter_async(2, 2, || {});
    let wa = flag_waker(&EVICTED_A);
    let cx_a = &mut std::task::Context::from_waker(&wa);
    assert!(
        core::pin::Pin::new(&mut fut_a).poll(cx_a).is_pending(),
        "future A parks behind the slot"
    );

    let mut fut_b = node.enter_async(3, 3, || {});
    let wb = flag_waker(&EVICTED_B);
    let cx_b = &mut std::task::Context::from_waker(&wb);
    assert!(
        core::pin::Pin::new(&mut fut_b).poll(cx_b).is_pending(),
        "future B parks in the evicted slot"
    );
    assert!(
        EVICTED_A.load(Ordering::SeqCst),
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
    static PARKED: AtomicBool = AtomicBool::new(false);

    // Regression: a future dropped while parked left its waker in the
    // queue forever — a slot nobody will ever wake, starving later
    // admissions (head-of-line blocking by a dead task).
    let node = SnubberNode::builder("async/drop_waiter")
        .max_in_flight(1)
        .build();
    let _slot = node.enter(1, 1, || {}).expect("occupy the only slot");

    let mut fut = node.enter_async(2, 2, || {});
    let wa = flag_waker(&PARKED);
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
    assert!(PARKED.load(Ordering::SeqCst), "real waiter must be woken");
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

// Cancellation handoff (`async_engine/entry.rs:13-16`): a settle pops ONE
// winner and wakes it after unlocking. If that task is dropped before it is
// re-polled, its queue entry is already gone — without a handoff the next
// waiter sleeps forever on a node that has no further settle to wake it.
#[test]
fn future_dropped_after_settle_pop_hands_the_slot_to_the_next_waiter() {
    static HANDOFF_A: AtomicBool = AtomicBool::new(false);
    static HANDOFF_B: AtomicBool = AtomicBool::new(false);

    HANDOFF_A.store(false, Ordering::SeqCst);
    HANDOFF_B.store(false, Ordering::SeqCst);
    let node = SnubberNode::builder("async/handoff")
        .max_in_flight(1)
        .build();
    let held = node.enter(1, 1, || {}).expect("hold the only slot");

    let wa = flag_waker(&HANDOFF_A);
    let wb = flag_waker(&HANDOFF_B);
    let mut cx_a = std::task::Context::from_waker(&wa);
    let mut cx_b = std::task::Context::from_waker(&wb);
    let mut fut_a = Box::pin(node.enter_async(2, 2, || {}));
    let mut fut_b = Box::pin(node.enter_async(3, 3, || {}));
    assert!(fut_a.as_mut().poll(&mut cx_a).is_pending(), "A parks");
    assert!(fut_b.as_mut().poll(&mut cx_b).is_pending(), "B parks");
    assert_eq!(node.waiter_len(), 2);

    // Settle pops A (the oldest) and wakes it — exactly one winner.
    held.commit();
    assert!(
        HANDOFF_A.load(Ordering::SeqCst),
        "settle wakes the oldest waiter"
    );
    assert!(
        !HANDOFF_B.load(Ordering::SeqCst),
        "a settle must not wake two waiters"
    );

    // A's task is cancelled in the gap before re-polling. Its entry was
    // already popped, so Drop sees the free slot and hands it to B.
    drop(fut_a);
    assert!(
        HANDOFF_B.load(Ordering::SeqCst),
        "a cancelled winner must hand the slot to the next waiter"
    );
    match fut_b.as_mut().poll(&mut cx_b) {
        std::task::Poll::Ready(Ok(g)) => {
            g.commit();
        }
        _ => panic!("B must be admitted after the handoff"),
    }
}

// A HalfOpen probe that parks (bulkhead full) releases the probe ROLE
// before sleeping; cancelling it must not leave the breaker wedged in
// HalfOpen with no admissible probe.
#[test]
fn half_open_async_probe_cancelled_in_queue_does_not_wedge_the_breaker() {
    static PROBE: AtomicBool = AtomicBool::new(false);

    use std::task::Context;

    let node = SnubberNode::builder("async/half-open-cancel")
        .max_in_flight(1)
        .trip_after_failures(1)
        .build();
    let held = node.enter(1, 1, || {}).expect("hold the slot");
    node.report(FaultKind::Panic, 2, 2); // Closed -> HalfOpen
    assert_eq!(node.stats().breaker_state, BreakerState::HalfOpen);

    let wa = flag_waker(&PROBE);
    let mut cx = Context::from_waker(&wa);
    let mut probe = Box::pin(node.enter_async(3, 3, || {}));
    assert!(
        probe.as_mut().poll(&mut cx).is_pending(),
        "the probe parks behind the held slot"
    );
    // Cancelled while parked: the probe flag was cleared when it parked.
    drop(probe);
    held.commit();
    assert_eq!(
        node.stats().breaker_state,
        BreakerState::HalfOpen,
        "a non-probe commit must not close the breaker"
    );

    // A fresh admission must still be able to probe and close the breaker.
    let guard = node
        .enter(4, 4, || {})
        .expect("a fresh half-open probe must be admissible");
    guard.commit();
    assert_eq!(
        node.stats().breaker_state,
        BreakerState::Closed,
        "the breaker must close on a successful probe"
    );
}

// Regression (0.6.4): async EvictOldest evictions now go through the same
// `record_locked` + per-kind policy path as the sync arm — `reported`
// agrees with `stats.evicted` (pre-fix the async arm bumped only the raw
// `evicted` counter, so a user's `.policy(EvictedOldest, ..)` never fired
// and the telemetry columns disagreed).
#[test]
fn async_eviction_reports_evicted_oldest_telemetry() {
    static REPORT_A: AtomicBool = AtomicBool::new(false);
    static REPORT_B: AtomicBool = AtomicBool::new(false);

    REPORT_A.store(false, Ordering::SeqCst);
    REPORT_B.store(false, Ordering::SeqCst);
    EVICT_HOOK_HITS.store(0, Ordering::SeqCst);
    let node = SnubberNode::builder("async/evict-telemetry")
        .max_in_flight(1)
        .queue_limit(1)
        .overflow(OverflowPolicy::EvictOldest)
        .on_fault(|record, _, _| {
            if record.kind == FaultKind::EvictedOldest {
                EVICT_HOOK_HITS.fetch_add(1, Ordering::SeqCst);
            }
        })
        .build();
    let _slot = node.enter(1, 1, || {}).expect("occupy the only slot");

    let mut fut_a = node.enter_async(2, 2, || {});
    let wa = flag_waker(&REPORT_A);
    let cx_a = &mut std::task::Context::from_waker(&wa);
    assert!(core::pin::Pin::new(&mut fut_a).poll(cx_a).is_pending());

    let mut fut_b = node.enter_async(3, 3, || {});
    let wb = flag_waker(&REPORT_B);
    let cx_b = &mut std::task::Context::from_waker(&wb);
    assert!(core::pin::Pin::new(&mut fut_b).poll(cx_b).is_pending());
    assert!(REPORT_A.load(Ordering::SeqCst), "eviction fired");

    let s = node.stats();
    assert_eq!(s.evicted, 1, "exactly one eviction, no double counting");
    assert_eq!(
        EVICT_HOOK_HITS.load(Ordering::SeqCst),
        1,
        "the async eviction must fire the per-kind policy path (sync-arm parity)"
    );
}

#[test]
fn paced_enter_skips_the_sleep_for_advise_only_nodes() {
    // AdviseOnly / Custom nodes have friction_wait_ms == 0: the paced
    // path must admit WITHOUT ever calling the sleeper.
    static SLEPT: AtomicU64 = AtomicU64::new(0);
    static SLEPT_MS: AtomicU64 = AtomicU64::new(0);
    let node = SnubberNode::builder("async/pace/advise")
        .backpressure(snubber::BackpressurePolicy::AdviseOnly)
        .max_in_flight(1)
        .build();
    let _hold = node.enter(1, 1, || {}).expect("hold the only slot");
    assert_eq!(node.friction_wait_ms(), 0, "AdviseOnly never paces");
    let g = block_on(node.enter_paced_async(
        2,
        2,
        || {},
        |ms| async move {
            SLEPT.fetch_add(1, Ordering::SeqCst);
            SLEPT_MS.fetch_add(ms, Ordering::SeqCst);
        },
    ));
    // AdviseOnly refuses honestly when full (never parks the caller) —
    // the point here is that the paced path NEVER calls the sleeper.
    assert!(g.is_err(), "AdviseOnly full bulkhead refuses");
    assert_eq!(SLEPT.load(Ordering::SeqCst), 0, "sleeper must not run");
    assert_eq!(SLEPT_MS.load(Ordering::SeqCst), 0);
}

#[test]
fn paced_enter_sleeps_exactly_the_advertised_wait_then_admits() {
    // Full AdaptiveBlock node: the paced path must sleep the advertised
    // friction_wait_ms and admit once the sleeper "frees" the slot.
    static SLEPT_MS: AtomicU64 = AtomicU64::new(0);
    let node = SnubberNode::builder("async/pace/admit")
        .backpressure(snubber::BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 50,
            shed_deeper_than: None,
        })
        .max_in_flight(1)
        .build();
    let hold = node.enter(1, 1, || {}).expect("hold the only slot");
    let want = node.friction_wait_ms();
    assert!(want > 0, "spring loaded when full");
    let n2 = node.clone();
    let g = block_on(node.enter_paced_async(
        2,
        2,
        || {},
        move |ms| {
            let n = n2.clone();
            async move {
                SLEPT_MS.fetch_add(ms, Ordering::SeqCst);
                // The timer IS the release: when it fires, the slot frees.
                hold.commit();
                let _ = n;
            }
        },
    ));
    assert!(g.is_ok(), "freed-while-paced must admit");
    assert_eq!(
        SLEPT_MS.load(Ordering::SeqCst),
        want,
        "sleeper must receive the advertised friction_wait_ms"
    );
}

#[test]
fn async_rate_shedding_files_its_own_kind() {
    // Parity with the sync path: a token-bucket refusal is reported as
    // `RateLimited`, never as `BulkheadFull` — the two share no policy row.
    let node = SnubberNode::builder("async/bucket").rate_per_sec(1).build();
    block_on(node.enter_async(1, 1, || {}))
        .expect("token 1")
        .commit();
    match block_on(node.enter_async(2, 2, || {})) {
        Err(EnterError::RateLimited) => {}
        Ok(_) => panic!("bucket empty must rate-limit"),
        Err(other) => panic!("expected RateLimited, got {other:?}"),
    }
    let s = node.stats();
    assert_eq!(s.rate_rejected, 1);
    assert_eq!(s.reported[FaultKind::RateLimited.index()], 1);
    assert_eq!(s.reported[FaultKind::BulkheadFull.index()], 0);
    assert_eq!(s.bulkhead_rejections, 0);
}

/// A future that PARKED and is then refused by a pre-lock gate (memory
/// pressure) must leave no queue entry behind: `EnterFuture::poll` retires
/// its own entry on every terminal `Ready`, so `queue_limit` is not eaten by
/// a ghost waiter that no settle will ever pop.
#[test]
fn a_parked_future_refused_on_memory_drains_its_queue_entry() {
    static MEM_WOKEN: AtomicBool = AtomicBool::new(false);

    use std::task::Context;

    let node = SnubberNode::builder("async/memory-drain")
        .max_in_flight(1)
        .queue_limit(4)
        .memory_limits(8, 64)
        .build();
    let held = node.enter(1, 1, || {}).expect("hold the only slot");

    let waker = flag_waker(&MEM_WOKEN);
    let mut cx = Context::from_waker(&waker);
    let mut parked = Box::pin(node.enter_async(2, 2, || {}));
    assert!(
        parked.as_mut().poll(&mut cx).is_pending(),
        "a full bulkhead parks the waker"
    );
    assert_eq!(node.waiter_len(), 1, "one waiter in the queue");

    // Pressure arrives AFTER the park, so the refusal is a terminal `Ready`
    // on a future that owns a live queue entry.
    node.note_memory_kb(128);
    assert!(node.stats().memory_pressure, "128 KiB over 64 hard");
    match parked.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Err(EnterError::MemoryPressure)) => {}
        std::task::Poll::Ready(Err(other)) => panic!("expected MemoryPressure, got {other:?}"),
        std::task::Poll::Ready(Ok(_)) => panic!("a draining node must admit nothing"),
        std::task::Poll::Pending => panic!("a draining node must not re-park"),
    }
    assert_eq!(
        node.waiter_len(),
        0,
        "the refused future retired its own entry — no ghost squatting queue_limit"
    );
    assert_eq!(node.queue_depth(), 0, "the queue is honestly empty");

    // And the node still admits once the slot frees: the parked rollback
    // never ran, so nothing was burnt by the refusal. The readable sample
    // under the soft limit is what lifts the flag — the refusal never
    // clears it by itself.
    drop(parked);
    held.commit();
    node.note_memory_kb(4);
    assert!(
        !node.stats().memory_pressure,
        "a sample under the soft limit ends the drain"
    );
    let guard = node
        .enter(3, 3, || {})
        .expect("a memory refusal leaves the node usable");
    guard.commit();
}

/// Same law for a generation bump: the epoch gate refuses the parked future
/// AFTER it queued, and the entry it owned must go with the verdict.
#[test]
fn a_parked_future_refused_on_a_stale_epoch_drains_its_queue_entry() {
    static STALE_WOKEN: AtomicBool = AtomicBool::new(false);

    use snubber::TransactionHeader;
    use std::task::Context;

    let node = SnubberNode::builder("async/stale-drain")
        .max_in_flight(1)
        .queue_limit(4)
        .build();
    let held = node.enter(1, 1, || {}).expect("hold the only slot");

    let waker = flag_waker(&STALE_WOKEN);
    let mut cx = Context::from_waker(&waker);
    let mut parked = Box::pin(node.enter_async(2, 2, || {}));
    match parked.as_mut().poll(&mut cx) {
        std::task::Poll::Pending => {}
        std::task::Poll::Ready(Err(e)) => panic!("a full bulkhead must park, got Err({e:?})"),
        std::task::Poll::Ready(Ok(_)) => panic!("a full bulkhead must park, got admission"),
    }
    assert_eq!(node.waiter_len(), 1, "one waiter in the queue");

    // The generation moves under the parked future (re-handshake).
    assert!(
        node.observe(&TransactionHeader::new(5, 9)),
        "a bump is always fresh"
    );
    match parked.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Err(EnterError::StaleEpoch)) => {}
        std::task::Poll::Ready(Err(other)) => panic!("expected StaleEpoch, got {other:?}"),
        std::task::Poll::Ready(Ok(_)) => panic!("a stale generation must admit nothing"),
        std::task::Poll::Pending => panic!("a stale future must not re-park"),
    }
    assert_eq!(
        node.waiter_len(),
        0,
        "the refused future retired its own entry — no ghost squatting queue_limit"
    );

    // A newer epoch than the adopted one is still admissible: the refusal
    // burned no coordinates.
    drop(parked);
    held.commit();
    let guard = node
        .enter(6, 6, || {})
        .expect("an epoch ahead of the adopted one admits");
    guard.commit();
}

/// The wake path itself, under concurrency: every thread polls ONLY when its
/// own waker fires, so a wake that is lost, or handed to a waker a later
/// re-park replaced, shows up as the bounded deadline expiring. A busy
/// re-poll loop cannot pass this test by accident — it never waits.
#[test]
#[cfg_attr(
    miri,
    ignore = "the assertion is a WALL-CLOCK property (a wake must land within 5 s of \
              real time) and Miri interprets the holder's spin budget, so the deadline \
              expires while the wake path is behaving: measured on the CI-pinned \
              nightly-2026-10-09, four threads stranded at iteration 0 after 169.24 s \
              with the other 19 lanes of this suite passing. Miri still audits this \
              queue's atomics through those lanes; the concurrent park/wake is audited \
              natively by the release suite."
)]
fn concurrent_parked_wakers_all_wake_inside_their_deadline() {
    const THREADS: usize = 6;
    const PER_THREAD: usize = 25;
    const WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    // Spins spent inside the guard, holding the slot. Without this the
    // churn is so cheap that two slots never fill and nobody ever parks —
    // the run then measures nothing, which the `PARKED` floor below pins.
    const HOLD_SPINS: u32 = 50_000;

    use std::sync::Arc;
    use std::task::{Context, Poll};

    // A `fn` item, not a closure: the retry below rebuilds the same future
    // type, and no two closure literals share a type.
    fn noop() {}

    let node = SnubberNode::builder("async/waker-churn")
        .max_in_flight(2)
        .queue_limit(64)
        .trip_after_failures(u32::MAX)
        .build();
    // Global epoch allocator: coordinates only ever move forward, so the
    // only refusals this run may see are races, never a mis-stamped header.
    let epoch_alloc = Arc::new(AtomicU64::new(1));
    let admitted = Arc::new(AtomicU64::new(0));
    let stale_retries = Arc::new(AtomicU64::new(0));
    let parked = Arc::new(AtomicU64::new(0));

    let mut joins = Vec::new();
    for t in 0..THREADS {
        let node = node.clone();
        let epoch_alloc = Arc::clone(&epoch_alloc);
        let admitted = Arc::clone(&admitted);
        let stale_retries = Arc::clone(&stale_retries);
        let parked = Arc::clone(&parked);
        joins.push(std::thread::spawn(move || {
            // One flag per thread; the raw vtable needs a `&'static`
            // address and only this thread ever reads it.
            let fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
            let waker = flag_waker(fired);
            let mut cx = Context::from_waker(&waker);
            for i in 0..PER_THREAD {
                let epoch = epoch_alloc.fetch_add(1, Ordering::SeqCst);
                let mut fut = Box::pin(node.enter_async(epoch, epoch * 8 + t as u64, noop));
                let deadline = std::time::Instant::now() + WAIT_BUDGET;
                loop {
                    // Clear BEFORE the poll: a wake that lands between the
                    // park and this store must still be seen.
                    fired.store(false, Ordering::SeqCst);
                    match fut.as_mut().poll(&mut cx) {
                        Poll::Ready(Ok(guard)) => {
                            for _ in 0..HOLD_SPINS {
                                std::hint::spin_loop();
                            }
                            guard.commit();
                            admitted.fetch_add(1, Ordering::SeqCst);
                            break;
                        }
                        Poll::Ready(Err(EnterError::StaleEpoch)) => {
                            stale_retries.fetch_add(1, Ordering::SeqCst);
                            let epoch = epoch_alloc.fetch_add(1, Ordering::SeqCst);
                            fut = Box::pin(node.enter_async(epoch, epoch * 8 + t as u64, noop));
                        }
                        Poll::Ready(Err(other)) => {
                            panic!("thread {t} iteration {i}: unexpected refusal {other:?}")
                        }
                        Poll::Pending => {
                            parked.fetch_add(1, Ordering::SeqCst);
                            while !fired.load(Ordering::SeqCst) {
                                if std::time::Instant::now() >= deadline {
                                    panic!(
                                        "thread {t} iteration {i}: parked and never woken inside {WAIT_BUDGET:?}"
                                    );
                                }
                                std::thread::yield_now();
                            }
                        }
                    }
                }
            }
        }));
    }
    for j in joins {
        j.join().expect("a parked waker stranded its task");
    }

    // The lane only proves something if waiters actually queued.
    assert!(
        parked.load(Ordering::SeqCst) >= THREADS as u64,
        "six threads over two slots must park, else no wake is being tested"
    );
    assert_eq!(
        admitted.load(Ordering::SeqCst),
        (THREADS * PER_THREAD) as u64,
        "every task was admitted"
    );
    assert_eq!(node.waiter_len(), 0, "no waiter outlived the churn");
    assert_eq!(node.queue_depth(), 0, "the queue is honestly empty");
    let s = node.stats();
    assert_eq!(s.committed, (THREADS * PER_THREAD) as u64);
    assert_eq!(s.implicit_drops, 0, "every guard settled");
    assert_eq!(
        s.stale_epochs_rejected,
        stale_retries.load(Ordering::SeqCst),
        "every race retry is booked exactly once"
    );
}
