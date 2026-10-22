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

use crate::node::{self, EnterError, NanoClock, SnubberNode, TrackedGuard};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Future of one async admission (`node.enter_async(...)`).
#[must_use = "futures do nothing unless you `.await` or poll them"]
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
            enqueued_at_ms: node::now_ms(),
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

/// Profiled variant of [`EnterFuture`]: arms an
/// [`ExecutionProfile`](crate::ExecutionProfile) on the guard the moment
/// admission succeeds.
///
/// The default `enter_async*` future carries no profile fields and reads
/// no clock. Here the wall clock is read at construction and at the
/// successful poll; `admit_us` therefore covers the queue wait.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ProfiledEnterFuture<F: FnOnce()> {
    inner: EnterFuture<F>,
    wall: NanoClock,
    cpu: Option<NanoClock>,
    t_enter: u64,
}

impl<F: FnOnce() + Unpin> ProfiledEnterFuture<F> {
    fn new(inner: EnterFuture<F>, wall: NanoClock, cpu: Option<NanoClock>) -> Self {
        Self {
            inner,
            wall,
            cpu,
            t_enter: wall(),
        }
    }
}

impl<F: FnOnce() + Unpin> Future for ProfiledEnterFuture<F> {
    type Output = Result<TrackedGuard<F>, EnterError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll(cx) {
            Poll::Ready(Ok(mut guard)) => {
                let t_admit = (this.wall)();
                let cpu_admit = this.cpu.map(|f| f()).unwrap_or(0);
                guard.arm_profile(this.wall, this.cpu, this.t_enter, t_admit, cpu_admit);
                Poll::Ready(Ok(guard))
            }
            other => other,
        }
    }
}

impl SnubberNode {
    /// Async admission with an execution profile (`std` clock, explicit ids).
    ///
    /// Same queue/waker machinery as [`SnubberNode::enter_async`]; the
    /// profile arms on the successful poll. `F` must be `Unpin` (same
    /// requirement as awaiting the plain future).
    #[track_caller]
    pub fn enter_async_profiled<F: FnOnce() + Unpin>(
        &self,
        epoch: u64,
        task_id: u64,
        rollback: F,
    ) -> ProfiledEnterFuture<F> {
        self.enter_async_profiled_with_clock(epoch, task_id, node::std_mono_ns, None, rollback)
    }

    /// Async admission with an execution profile and auto sequence ids.
    #[track_caller]
    pub fn enter_async_auto_profiled<F: FnOnce() + Unpin>(
        &self,
        rollback: F,
    ) -> ProfiledEnterFuture<F> {
        let seq = self.next_sequence();
        self.enter_async_profiled(seq, seq, rollback)
    }

    /// Async admission with injected nanosecond clocks (fake-clock tests).
    #[track_caller]
    pub fn enter_async_profiled_with_clock<F: FnOnce() + Unpin>(
        &self,
        epoch: u64,
        task_id: u64,
        wall: NanoClock,
        cpu: Option<NanoClock>,
        rollback: F,
    ) -> ProfiledEnterFuture<F> {
        let loc = core::panic::Location::caller();
        let inner = EnterFuture::new(self.clone(), epoch, task_id, rollback, loc);
        ProfiledEnterFuture::new(inner, wall, cpu)
    }
}
