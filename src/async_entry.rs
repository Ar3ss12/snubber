//! Async entry queue: `enter_async()` on `core::task::Waker`, zero deps.
//!
//! When `max_in_flight` is full the future does NOT spin retries — it
//! parks its [`core::task::Waker`] in the node queue and sleeps at 0% CPU.
//! Any `commit/abort/drop` pops ONE waiter and wakes it OUTSIDE the lock
//! (PHASE B — foreign wake, never under the mutex).
//!
//! Works with Tokio, async-std, smol, embassy — anything polling `Future`.
//! Queue is capped (1024, oldest evicted); atomic admission reserves the slot
//! and only consumes the rollback on success — no lost wakeup, no burnt `FnOnce`.

use super::node::{EnterError, SnubberNode, TrackedGuard};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Future of one async admission (`node.enter_async(...)`).
pub struct EnterFuture<F: FnOnce()> {
    node: SnubberNode,
    epoch: u64,
    task_id: u64,
    rollback: Option<F>,
    loc: &'static core::panic::Location<'static>,
    enqueued_at_ms: u64,
}

impl<F: FnOnce()> EnterFuture<F> {
    pub(crate) fn new(
        node: SnubberNode,
        epoch: u64,
        task_id: u64,
        rollback: F,
        loc: &'static core::panic::Location<'static>,
    ) -> Self {
        Self {
            node,
            epoch,
            task_id,
            rollback: Some(rollback),
            loc,
            enqueued_at_ms: super::node::now_ms(),
        }
    }
}

impl<F: FnOnce() + Unpin> Future for EnterFuture<F> {
    type Output = Result<TrackedGuard<F>, EnterError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.node.enter_async_attempt(
            this.epoch,
            this.task_id,
            this.loc,
            &mut this.rollback,
            cx.waker(),
            this.enqueued_at_ms,
        )
    }
}
