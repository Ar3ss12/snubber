//! Resource exoskeleton (Weapon 3): the guard holds the resource hostage.
//!
//! `core`-only, no `alloc`: pure generics over [`SnubberGuard`](crate::SnubberGuard).
//! The resource is reachable ONLY through [`Protected::get`]/[`get_mut`]
//! while the transaction lives; on `commit()` it walks out clean, on any
//! other exit `Drop` runs the rollback and the resource dies WITH the guard.
//! Touching hardware after a crash becomes a COMPILE error
//! (`use of moved value`), not a forgotten `if err`.
//!
//! Honest limit (documented, not hidden): `get()` can copy `Copy` data out
//! before failure — this raises the bar, it does not build a prison.
//! Non-`Copy` handles (sockets, Win32 `HANDLE`s) cannot escape except via
//! `commit()`, which is the whole point.
//!
//! Owner (R1): this module. Drop order: `guard` is declared FIRST so the
//! rollback body runs while `resource` is still alive (fields drop in
//! declaration order — rollback sees intact hardware, then it burns).

use super::SnubberGuard;

/// Guarded resource: reachable only inside the transaction.
///
/// Acquire with [`Protected::new`], work through [`get`](Self::get) /
/// [`get_mut`](Self::get_mut), release with [`commit`](Self::commit).
/// Any other exit (error, `?`, panic, plain drop) fires the rollback and
/// destroys the resource — there is nothing left to touch afterwards.
#[must_use = "a Protected resource burns with its guard — commit() to release it, never drop it sight unseen"]
pub struct Protected<T, F: FnOnce()> {
    /// Declared FIRST: drop runs the rollback while `resource` still lives.
    guard: Option<SnubberGuard<F>>,
    /// Declared SECOND: dies right after the rollback fired.
    resource: Option<T>,
}

impl<T, F: FnOnce()> Protected<T, F> {
    /// Cage the resource behind a freshly armed guard.
    #[inline]
    pub fn new(resource: T, guard: SnubberGuard<F>) -> Self {
        Self {
            guard: Some(guard),
            resource: Some(resource),
        }
    }

    /// Borrow the caged resource (read-only).
    #[inline]
    #[must_use]
    pub fn get(&self) -> &T {
        self.resource.as_ref().expect("protected resource live")
    }

    /// Borrow the caged resource (read-write).
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self) -> &mut T {
        self.resource.as_mut().expect("protected resource live")
    }

    /// Success: disarm the rollback, hand the resource back alive.
    ///
    /// The ONLY way to take `T` out. After this call the rollback is dead
    /// and the resource is yours — the transaction closed clean.
    #[inline]
    pub fn commit(mut self) -> T {
        if let Some(guard) = self.guard.take() {
            let _ = guard.commit();
        }
        self.resource.take().expect("protected resource live")
    }

    /// Planned failure: run the rollback NOW, destroy the resource.
    ///
    /// Returns the terminal event for logging; `T` is gone afterwards.
    #[inline]
    pub fn abort(mut self, reason: &'static str) -> super::SnubberEvent<()> {
        if let Some(guard) = self.guard.take() {
            let _ = guard.abort(reason);
        }
        self.resource.take();
        super::SnubberEvent::Aborted {
            header: super::TransactionHeader::new(0, 0),
            reason,
        }
    }
}

impl<T, F: FnOnce()> Drop for Protected<T, F> {
    #[inline]
    fn drop(&mut self) {
        // Guard first (rollback while hardware intact), resource second.
        // `Option::take` keeps this idempotent: `commit`/`abort` already
        // took their halves, double-drop is a no-op, never a double-fire.
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        self.resource.take();
    }
}
