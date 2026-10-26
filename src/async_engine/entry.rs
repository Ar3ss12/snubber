//! Async entry queue: `enter_async()` on `core::task::Waker`, zero deps.
//!
//! When `max_in_flight` is full the future does NOT spin retries — it
//! parks its [`core::task::Waker`] in the node queue and sleeps at 0% CPU.
//! Any `commit/abort/drop` pops ONE waiter and wakes it OUTSIDE the lock
//! (PHASE B — foreign wake, never under the mutex).
//!
//! Eviction and cancellation are honest: `OverflowPolicy::EvictOldest`
//! marks the evicted future's flag and WAKES it, so its next poll
//! resolves to `Err(EnterError::QueueFull)` instead of hanging forever;
//! dropping a parked future deregisters exactly its own queue entry.
//!
//! Cancellation handoff: a settle pops ONE winner and wakes it after
//! unlocking. If that task is dropped in the gap, `Drop for EnterFuture`
//! observes a free slot in `deregister_waiter` and wakes the next waiter,
//! so a quiescent node cannot strand the queue (no "next settle" needed).
//! `EnterFuture::poll` retires its own queue entry on every terminal
//! `Ready`, so an early refusal (memory/TTL/breaker/stale/queue-full/rate)
//! never leaves a ghost entry squatting `queue_limit`.
//!
//! Works with Tokio, async-std, smol, embassy — anything polling `Future`.
//! Queue is capped (`queue_limit`, default 32; the over-limit policy applies —
//! default `RejectNew`, so a full queue refuses, it never evicts). Atomic
//! admission reserves the slot and only consumes the rollback on success —
//! no lost wakeup, no burnt `FnOnce`.

use crate::node::{self, EnterError, NanoClock, SnubberNode, TrackedGuard};
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};
use std::sync::Arc;

/// Future of one async admission (`node.enter_async(...)`).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct EnterFuture<F: FnOnce()> {
    node: SnubberNode,
    epoch: u64,
    task_id: u64,
    rollback: Option<F>,
    loc: &'static core::panic::Location<'static>,
    /// TTL clock: stamped when the future first PARKS (first `waiters`
    /// entry), not at construction — an executor may buffer the future
    /// long before the first poll, and "expired in queue" must mean it
    /// really queued.
    enqueued_at_ms: Option<u64>,
    /// Identity of this future's parked queue entry (`Arc` pointer
    /// equality with the node's `ParkedWaiter.evicted`). `None` until the
    /// future actually parks — the ready admission path stays alloc-free.
    /// `Drop` and every re-park deregister through this flag, so one
    /// future owns at most one queue entry at any moment.
    evicted: Option<Arc<AtomicBool>>,
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
            enqueued_at_ms: None,
            evicted: None,
        }
    }
}

impl<F: FnOnce()> Drop for EnterFuture<F> {
    fn drop(&mut self) {
        // Deregister unconditionally: a spec-legal spurious re-poll can
        // admit this future while its queue entry is still parked, so a
        // `parked`-gated drop would leak a ghost waiter that consumes
        // queue capacity forever. `deregister_waiter` is identity-matched
        // and idempotent — the normal wake -> pop path pays a no-op scan.
        if let Some(flag) = &self.evicted {
            self.node.deregister_waiter(flag);
        }
    }
}

impl<F: FnOnce() + Unpin> Future for EnterFuture<F> {
    type Output = Result<TrackedGuard<F>, EnterError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this
            .evicted
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
        {
            // The evictor already popped our entry; the flag is the verdict,
            // not a live slot. Clear it so Drop does not wake an extra waiter.
            this.evicted = None;
            return Poll::Ready(Err(EnterError::QueueFull));
        }
        let mut evicted_slot: Option<Arc<AtomicBool>> = None;
        let attempt = this.node.enter_async_attempt(
            this.epoch,
            this.task_id,
            this.loc,
            &mut this.rollback,
            cx.waker(),
            &mut evicted_slot,
            this.evicted.as_ref(),
            &mut this.enqueued_at_ms,
        );
        if attempt.is_pending() {
            let new_flag = evicted_slot.take();
            if let Some(new) = &new_flag {
                // An executor whose waker differs per poll never matches
                // `will_wake`, so each full re-park pushes a second queue
                // entry. Retire our previous entry: one future owns
                // exactly one waiter slot at any moment.
                if let Some(old) = &this.evicted {
                    if !Arc::ptr_eq(old, new) {
                        this.node.deregister_waiter(old);
                    }
                }
            }
            this.evicted = new_flag;
            return attempt;
        }
        match &attempt {
            // Ready, but a spurious re-poll may have admitted us while our
            // own queue entry is still parked: retire whatever entry this
            // future still owns (identity-matched, no-op when the settle
            // path already popped it).
            Poll::Ready(Ok(_)) | Poll::Ready(Err(_)) => {
                if let Some(old) = this.evicted.take() {
                    this.node.deregister_waiter(&old);
                }
            }
            Poll::Pending => {}
        }
        attempt
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
