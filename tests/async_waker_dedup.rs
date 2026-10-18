use core::future::Future;
use core::task::{Context, RawWaker, RawWakerVTable, Waker};
use snubber::SnubberNode;

fn create_noop_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
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
    let waker = create_noop_waker();
    let mut cx = Context::from_waker(&waker);

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
