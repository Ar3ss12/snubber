//! Async entry queue in action: one slot, two tasks, zero spinning.
//!
//! Run: `cargo run --example async_waker_queue`
//! No runtime: a 15-line `block_on` polls the future with yields.

use snubber::SnubberNode;
use std::task::{RawWaker, RawWakerVTable, Waker};

fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VT)
    }
    fn wake(_: *const ()) {}
    fn wake_by_ref(_: *const ()) {}
    fn drop(_: *const ()) {}
    static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    let w: Waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VT)) };
    let mut cx = std::task::Context::from_waker(&w);
    let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
    loop {
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => return v,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn main() {
    let node = SnubberNode::builder("demo/waker").max_in_flight(1).build();
    let gate = node
        .enter(1, 1, || println!("  [rollback] slot 1 discharged"))
        .expect("slot");
    println!("[main] slot 1 held; waiters = {}", node.waiter_len());
    // Task B parks its waker: Pending, 0% CPU, no retry spam.
    let mut pending = node.enter_async(2, 2, || println!("  [rollback] slot 2 discharged"));
    let parked = {
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VT)
        }
        fn wake(_: *const ()) {}
        fn wake_by_ref(_: *const ()) {}
        fn drop(_: *const ()) {}
        static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        let w: Waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VT)) };
        let mut cx = std::task::Context::from_waker(&w);
        use std::future::Future;
        unsafe { std::pin::Pin::new_unchecked(&mut pending) }
            .poll(&mut cx)
            .is_pending()
    };
    println!(
        "[main] task B parked: {parked}; waiters = {}",
        node.waiter_len()
    );
    drop(pending); // parked future burnt its rollback on register — re-arm
    drop(gate); // slot freed + ONE waiter woken (outside the lock)
    let g2 = block_on(node.enter_async(2, 3, || println!("  [rollback] slot 2b discharged")))
        .expect("woken");
    println!(
        "[main] task B admitted after wake; waiters = {}",
        node.waiter_len()
    );
    // Async settle AFTER the work: guard crosses `.await` legally.
    let v = block_on(snubber::settle_async(g2, "demo_fail", async {
        Ok::<u32, &'static str>(42u32)
    }));
    println!("[main] settled with value: {v:?}");
    println!("[main] stats: {:?}", node.stats());
}
