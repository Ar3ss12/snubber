//! Integration tests for Task Engine:
//! Queue capacity, Task TTL, Checkpoints, and Stage inspection.

use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use snubber::{EnterError, OverflowPolicy, SnubberNode};

#[test]
fn task_ttl_expired_rejects_in_queue() {
    let node = SnubberNode::builder("test_task_ttl")
        .max_in_flight(1)
        .queue_capacity(4)
        .task_ttl(Duration::from_millis(20))
        .build();

    // 1. Fill the bulkhead slot
    let g1 = node.enter(1, 1, || {}).expect("first enters bulkhead");

    // 2. Queue an async waiter
    let fut = node.enter_async(2, 2, || {});
    let mut fut = Box::pin(fut);

    // Initial poll parks the waker
    let waker = futures_lite_fake_waker();
    let mut cx = core::task::Context::from_waker(&waker);
    assert!(fut.as_mut().poll(&mut cx).is_pending());

    // 3. Sleep past TTL (30ms > 20ms)
    std::thread::sleep(Duration::from_millis(30));

    // Release g1 so slot frees
    g1.commit();

    // 4. Next poll sees TTL expired!
    match fut.as_mut().poll(&mut cx) {
        core::task::Poll::Ready(Err(EnterError::QueueTtlExpired)) => {}
        _ => panic!("expected QueueTtlExpired"),
    }

    let stats = node.stats();
    assert_eq!(stats.committed, 1);
    let fault = stats.last_fault.expect("timeout fault recorded");
    assert_eq!(fault.kind, snubber::FaultKind::Timeout);
    assert!(fault.queue_wait_ms >= 20);
}

#[test]
fn queue_capacity_reject_new_blocks_excess_waiters() {
    let node = SnubberNode::builder("test_q_capacity")
        .max_in_flight(1)
        .queue_capacity(2)
        .shed_policy(OverflowPolicy::RejectNew)
        .build();

    let _g = node.enter(1, 1, || {}).expect("slot 1");

    let w1 = futures_lite_fake_waker_id(1);
    let mut cx1 = core::task::Context::from_waker(&w1);
    let mut f1 = Box::pin(node.enter_async(2, 2, || {}));
    assert!(f1.as_mut().poll(&mut cx1).is_pending());

    let w2 = futures_lite_fake_waker_id(2);
    let mut cx2 = core::task::Context::from_waker(&w2);
    let mut f2 = Box::pin(node.enter_async(3, 3, || {}));
    assert!(f2.as_mut().poll(&mut cx2).is_pending());

    // Queue is now full (2 waiters in queue). 3rd should be rejected with QueueFull!
    let w3 = futures_lite_fake_waker_id(3);
    let mut cx3 = core::task::Context::from_waker(&w3);
    let mut f3 = Box::pin(node.enter_async(4, 4, || {}));
    match f3.as_mut().poll(&mut cx3) {
        core::task::Poll::Ready(Err(EnterError::QueueFull)) => {}
        _ => panic!("expected QueueFull"),
    }

    assert_eq!(node.waiter_len(), 2);
}

#[test]
fn queue_capacity_evict_oldest_discards_stale_waiter() {
    let node = SnubberNode::builder("test_q_evict")
        .max_in_flight(1)
        .queue_capacity(2)
        .shed_policy(OverflowPolicy::EvictOldest)
        .build();

    let g = node.enter(1, 1, || {}).expect("slot 1");

    let w1 = futures_lite_fake_waker_id(1);
    let mut cx1 = core::task::Context::from_waker(&w1);
    let mut f1 = Box::pin(node.enter_async(2, 2, || {}));
    assert!(f1.as_mut().poll(&mut cx1).is_pending());

    let w2 = futures_lite_fake_waker_id(2);
    let mut cx2 = core::task::Context::from_waker(&w2);
    let mut f2 = Box::pin(node.enter_async(3, 3, || {}));
    assert!(f2.as_mut().poll(&mut cx2).is_pending());

    // 3rd waiter evicts f1
    let w3 = futures_lite_fake_waker_id(3);
    let mut cx3 = core::task::Context::from_waker(&w3);
    let mut f3 = Box::pin(node.enter_async(4, 4, || {}));
    assert!(f3.as_mut().poll(&mut cx3).is_pending());

    assert_eq!(node.waiter_len(), 2);
    let stats = node.stats();
    assert_eq!(stats.evicted, 1);

    drop(g);
}

#[test]
fn checkpoint_and_snapshot_telemetry() {
    let node = SnubberNode::builder("test_checkpoint").build();

    let mut guard = node.enter(1, 42, || {}).expect("entered");
    assert_eq!(guard.stage(), 0);
    assert_eq!(guard.stage_label(), None);

    guard.checkpoint(1, "connected");
    assert_eq!(guard.stage(), 1);
    assert_eq!(guard.stage_label(), Some("connected"));

    let snap = guard.state_snapshot();
    assert_eq!(snap.stage, 1);
    assert_eq!(snap.stage_label, Some("connected"));
    assert_eq!(snap.header.task_id, 42);

    guard.checkpoint(2, "sent_headers");
    assert_eq!(guard.stage(), 2);
    assert_eq!(guard.stage_label(), Some("sent_headers"));

    guard.commit();
}

#[test]
fn implicit_drop_records_stage_label() {
    static ROLLED_BACK: AtomicBool = AtomicBool::new(false);
    ROLLED_BACK.store(false, Ordering::SeqCst);

    let node = SnubberNode::builder("test_drop_stage").build();

    {
        let mut guard = node
            .enter(1, 100, || ROLLED_BACK.store(true, Ordering::SeqCst))
            .expect("entered");
        guard.checkpoint(3, "query_executing");
        std::thread::sleep(Duration::from_millis(5));
        // Drop without commit!
    }

    assert!(ROLLED_BACK.load(Ordering::SeqCst));
    let stats = node.stats();
    assert_eq!(stats.implicit_drops, 1);
    let fault = stats.last_fault.expect("fault recorded");
    assert_eq!(fault.stage, 3);
    assert_eq!(fault.stage_label, Some("query_executing"));
    // No watchdog is armed here, so the guard is deliberately unstamped
    // (`needs_stamp`): unarmed admissions never read the clock, and the
    // record carries no wall duration. Timing WITH a deadline is covered
    // by `wall_only_guard_times_out_without_sampler` in clock_pulse.rs.
    assert_eq!(fault.elapsed_ms, 0);
    assert_eq!(fault.retry_count, 0);
}

#[test]
fn retry_count_tracking_in_guard_and_snapshot() {
    let node = SnubberNode::builder("test_retries").build();

    let mut guard = node.enter(1, 10, || {}).expect("entered");
    assert_eq!(guard.retry_count(), 0);

    guard.set_retry_count(2);
    assert_eq!(guard.retry_count(), 2);

    let snap = guard.state_snapshot();
    assert_eq!(snap.retry_count, 2);

    guard.checkpoint(1, "retry_step");
    guard.commit();

    // Check with fluent builder on enter
    let guard2 = node
        .enter(2, 11, || {})
        .expect("entered 2")
        .with_retry_count(3);
    assert_eq!(guard2.retry_count(), 3);
    assert_eq!(guard2.state_snapshot().retry_count, 3);
    guard2.commit();
}

#[test]
fn payload_budget_enforced() {
    let node = SnubberNode::builder("test_budget")
        .payload_budget(1024)
        .build();

    assert_eq!(node.payload_budget_bytes(), 1024);

    // Payload within budget passes
    let g1 = node
        .enter_with_payload_budget(1, 1, 512, || {})
        .expect("within budget");
    g1.commit();

    // Payload exceeding budget fails fast before slot acquisition
    let res = node.enter_with_payload_budget(2, 2, 2048, || {});
    match res {
        Err(EnterError::PayloadBudgetExceeded(size)) => assert_eq!(size, 2048),
        _ => panic!("expected PayloadBudgetExceeded"),
    }

    let stats = node.stats();
    assert_eq!(stats.poisoned_rejected, 1);
    let fault = stats.last_fault.expect("fault recorded");
    assert_eq!(fault.kind, snubber::FaultKind::PoisonedPayload);
    assert_eq!(
        fault.detail,
        Some(snubber::FaultDetail::Static("payload budget exceeded"))
    );
}

#[test]
fn enter_async_auto_and_is_expired_test() {
    let node = SnubberNode::builder("test_auto_async")
        .timeout_ms(20)
        .build();

    let fut = node.enter_async_auto(|| {});
    let mut fut = Box::pin(fut);
    let waker = futures_lite_fake_waker();
    let mut cx = core::task::Context::from_waker(&waker);

    let guard = match fut.as_mut().poll(&mut cx) {
        core::task::Poll::Ready(Ok(g)) => g,
        _ => panic!("expected immediate admission"),
    };

    assert!(!guard.is_expired(), "fresh guard is not expired");
    assert!(guard.header().epoch > 0);

    // Sleep past deadline
    std::thread::sleep(Duration::from_millis(30));
    assert!(guard.is_expired(), "guard is expired after deadline");

    guard.commit();

    // AsyncGate admit_auto check
    let gate = snubber::AsyncGate::new(node).expect("gate created");
    let g2 = gate.admit_auto(|| {}).expect("admitted auto");
    g2.commit();
}

fn futures_lite_fake_waker() -> core::task::Waker {
    futures_lite_fake_waker_id(0)
}

fn futures_lite_fake_waker_id(id: usize) -> core::task::Waker {
    use std::sync::Arc;
    use std::task::Wake;

    /// Each `Arc::new` is a distinct allocation, so the waker data
    /// pointer (which `will_wake` compares) is unique per id — no raw
    /// vtable, no provenance tricks, valid under `-Zmiri-strict-provenance`.
    struct FakeWake(usize);
    impl Wake for FakeWake {
        fn wake(self: Arc<Self>) {
            // Reading the id keeps the field live: the payload itself is
            // irrelevant, the distinct `Arc` allocation is the identity.
            let _id = self.0;
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let _id = self.0;
        }
    }
    core::task::Waker::from(Arc::new(FakeWake(id)))
}

#[test]
fn fake_waker_identity_matches_allocation() {
    let w1 = futures_lite_fake_waker_id(1);
    let w2 = futures_lite_fake_waker_id(2);
    // Both Arcs are live at once, so their addresses cannot alias; the
    // engine's dedup compares exactly this `will_wake` identity.
    assert!(!w1.will_wake(&w2), "distinct allocations must not match");
    // std builds `Waker::from(Arc<W>)` and its clone vtable as two
    // structurally identical const-promoted `RawWakerVTable`s and relies on
    // LLVM to merge them, so a clone keeps the same vtable pointer natively.
    // Miri never runs LLVM: each promotion stays its own allocation, so a
    // cloned std waker can carry a different vtable and `will_wake`
    // (documented as best-effort) returns false. That is a Miri artifact,
    // not engine behavior — the engine dedups on its own `Arc<AtomicBool>`
    // identity and never assumes cloned wakers compare equal.
    #[cfg(not(miri))]
    assert!(w1.will_wake(&w1.clone()), "a cloned waker keeps identity");
}

#[test]
#[cfg_attr(miri, ignore = "wall-clock TTL margins; run natively")]
fn spurious_repolls_do_not_reset_the_ttl_clock() {
    // Regression: the dedup re-park path re-stamped `enqueued_at_ms`, so a
    // future parked in the queue never accumulated queue-wait — an
    // executor spinning re-polls starved `task_ttl` forever. The TTL
    // origin must stay at the FIRST park.
    let ttl = Duration::from_millis(100);
    let node = SnubberNode::builder("test_ttl_restamp")
        .max_in_flight(1)
        .queue_capacity(4)
        .task_ttl(ttl)
        .build();
    let g1 = node.enter(1, 1, || {}).expect("first enters bulkhead");

    let mut fut = Box::pin(node.enter_async(2, 2, || {}));
    let waker = futures_lite_fake_waker();
    let mut cx = core::task::Context::from_waker(&waker);

    // First park stamps the origin (t = 0).
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    let parked_at = std::time::Instant::now();
    // The product compares a 1 ms clock with a strict `wait_ms > ttl`, so
    // observations within a tick of the deadline are ambiguous. `pad` is
    // the tolerance for that truncation.
    let pad = Duration::from_millis(10);
    // Spurious re-polls with the same waker hit the dedup path; each must
    // leave the origin untouched. A stalled host can oversleep past the
    // deadline, so the poll may already expire; both outcomes are accepted
    // here, but any expiry has to report a wait spanning the FIRST park.
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(30));
        match fut.as_mut().poll(&mut cx) {
            core::task::Poll::Pending => {}
            core::task::Poll::Ready(Err(EnterError::QueueTtlExpired)) => {
                let fault = node.stats().last_fault.expect("ttl fault recorded");
                let elapsed_ms = parked_at.elapsed().as_millis() as u64;
                assert!(
                    fault.queue_wait_ms + pad.as_millis() as u64 >= elapsed_ms,
                    "queue_wait_ms {} must span the parked time {elapsed_ms} ms",
                    fault.queue_wait_ms
                );
                return;
            }
            core::task::Poll::Ready(_) => panic!("spurious re-poll must not admit"),
        }
    }
    // Now t >= ttl + pad > TTL: the next poll must expire, measured from
    // the FIRST park — not from the last spurious re-poll.
    while parked_at.elapsed() < ttl + pad {
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(g1);
    match fut.as_mut().poll(&mut cx) {
        core::task::Poll::Ready(Err(EnterError::QueueTtlExpired)) => {}
        _ => panic!("spurious re-polls must not reset the TTL clock"),
    }
    let fault = node.stats().last_fault.expect("ttl fault recorded");
    let elapsed_ms = parked_at.elapsed().as_millis() as u64;
    assert!(
        fault.queue_wait_ms >= ttl.as_millis() as u64,
        "queue_wait_ms must span the whole parked time, got {}",
        fault.queue_wait_ms
    );
    assert!(
        fault.queue_wait_ms + pad.as_millis() as u64 >= elapsed_ms,
        "queue_wait_ms {} must be measured from the first park, not the last re-poll ({elapsed_ms} ms parked)",
        fault.queue_wait_ms
    );
}

#[test]
fn queue_gauges_report_waiter_buffer() {
    let node = SnubberNode::builder("queue_gauge")
        .queue_capacity(8)
        .build();
    assert_eq!(node.queue_capacity(), 8);
    assert_eq!(node.queue_depth(), 0);
}
