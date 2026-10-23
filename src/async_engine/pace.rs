//! Async pacing: Backpressure delay without blocking executor threads.
//!
//! Sync `AdaptiveBlock` parks the OS thread (`std::thread::sleep`) — in an
//! async runtime that freezes the WHOLE worker. This module injects YOUR
//! sleep (`tokio::time::sleep`, `async-std`, test fake): snubber computes
//! the friction wait using configured backpressure friction, then `.await`s YOUR timer.
//!
//! Zero deps: `Future` is `core`, the sleeper is a caller-supplied async fn.
//!
//! Bounds: `AdviseOnly` / `Custom` nodes never sleep here (`friction_wait_ms`
//! is 0 for them). An `AdaptiveBlock` node paces through YOUR timer, but if
//! the bulkhead is STILL full at the final admit, `enter()` runs its own
//! bounded park (`std::thread::sleep`, capped by `max_wait_ms`) — on an
//! executor that blocks the worker thread. For zero-blocking admission on
//! `AdaptiveBlock` use [`crate::EnterFuture`] (`enter_async`), which parks
//! the `Waker` instead.

use crate::node::{EnterError, SnubberNode, TrackedGuard};

/// Admit with an async pace: compute the friction wait, sleep in YOUR
/// runtime, then admit once. `sleep_fn(wait_ms)` is YOUR timer
/// (e.g. `|ms| tokio::time::sleep(Duration::from_millis(ms))`).
/// `AdviseOnly` / `Custom` nodes skip the sleep (`friction_wait_ms` = 0).
/// Caution on `AdaptiveBlock`: if the bulkhead is still full at the final
/// admit, `enter()` parks the OS thread (bounded by `max_wait_ms`) — use
/// `enter_async` when the executor thread must never block.
pub async fn enter_paced_async<F, Sf, SfFut>(
    node: &SnubberNode,
    epoch: u64,
    task_id: u64,
    rollback: F,
    sleep_fn: Sf,
) -> Result<TrackedGuard<F>, EnterError>
where
    F: FnOnce(),
    Sf: FnOnce(u64) -> SfFut,
    SfFut: core::future::Future<Output = ()>,
{
    let wait = node.friction_wait_ms();
    if wait > 0 {
        sleep_fn(wait).await;
    }
    node.enter(epoch, task_id, rollback)
}
