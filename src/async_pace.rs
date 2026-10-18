//! Async pacing: Backpressure delay without blocking executor threads.
//!
//! Sync `AdaptiveBlock` parks the OS thread (`std::thread::sleep`) — in an
//! async runtime that freezes the WHOLE worker. This module injects YOUR
//! sleep (`tokio::time::sleep`, `async-std`, test fake): snubber computes
//! the friction wait using configured backpressure friction, then `.await`s YOUR timer.
//!
//! Zero deps: `Future` is `core`, the sleeper is a caller-supplied async fn.

use super::node::{EnterError, SnubberNode, TrackedGuard};

/// Admit with an async pace: compute the friction wait, sleep in YOUR
/// runtime, then admit once. `sleep_fn(wait_ms)` is YOUR timer
/// (e.g. `|ms| tokio::time::sleep(Duration::from_millis(ms))`).
/// `AdviseOnly` nodes skip the sleep (nothing to pace — admit now).
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
