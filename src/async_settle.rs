//! Async settle: run the fallible work future, THEN settle the guard.
//!
//! `TrackedGuard` owns NO mutex (only `Arc` + coords), so it legally lives
//! across `.await` points to the end of the function — commit AFTER the
//! work, never before. This module is the honest two-step:
//!
//! ```text
//! match work.await { Ok(v) => { guard.commit(); v } Err(e) => { guard.abort(reason); e } }
//! ```
//!
//! Cancellation safety: if `work.await` is cancelled by the executor (e.g. `tokio::select!`),
//! the dropped `guard` runs its emergency rollback via RAII without leaking slots or state.
//!
//! No runtime inside, no `block_on` — the caller `.await`s in ITS runtime.

use super::node::TrackedGuard;

/// Run `work` to completion, then settle: `Ok(v)` → `commit()` + `v`,
/// `Err(e)` → `abort(reason)` + `e`. The guard crosses the `.await` legally
/// (no lock held between) — success is recorded on a LIVE operation only.
pub async fn settle_async<F, Fut, T, E>(
    guard: TrackedGuard<F>,
    reason: &'static str,
    work: Fut,
) -> Result<T, E>
where
    F: FnOnce(),
    Fut: core::future::Future<Output = Result<T, E>>,
{
    match work.await {
        Ok(v) => {
            guard.commit();
            Ok(v)
        }
        Err(e) => {
            guard.abort(reason);
            Err(e)
        }
    }
}
