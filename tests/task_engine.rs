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
fn implicit_drop_records_stage_label_and_timing() {
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
    assert!(fault.elapsed_ms >= 4);
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
    struct FakeWake(#[allow(dead_code)] usize);
    impl Wake for FakeWake {
        fn wake(self: Arc<Self>) {}
        fn wake_by_ref(self: &Arc<Self>) {}
    }
    core::task::Waker::from(Arc::new(FakeWake(id)))
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
    // Spurious re-polls with the same waker hit the dedup path; each must
    // leave the origin untouched (t stays well below TTL after the sleeps).
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(30));
        if parked_at.elapsed() >= ttl {
            // A stalled host overslept past the TTL: expiry is then the
            // honest verdict (measured from the first park), so the
            // pending window cannot be observed on this run.
            match fut.as_mut().poll(&mut cx) {
                core::task::Poll::Ready(Err(EnterError::QueueTtlExpired)) => return,
                _ => panic!("expired TTL must surface as QueueTtlExpired"),
            }
        }
        assert!(fut.as_mut().poll(&mut cx).is_pending());
    }
    // Now t >= 110 ms > TTL = 100 ms: the next poll must expire, measured
    // from the FIRST park — not from the last spurious re-poll.
    while parked_at.elapsed() < Duration::from_millis(110) {
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(g1);
    match fut.as_mut().poll(&mut cx) {
        core::task::Poll::Ready(Err(EnterError::QueueTtlExpired)) => {}
        _ => panic!("spurious re-polls must not reset the TTL clock"),
    }
    let fault = node.stats().last_fault.expect("ttl fault recorded");
    assert!(
        fault.queue_wait_ms >= ttl.as_millis() as u64,
        "queue_wait_ms must span the whole parked time, got {}",
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
