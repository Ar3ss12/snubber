//! Scoped resource encapsulation with transactional rollback guarantees.
//!
//! Provides [`Protected`]: a RAII wrapper that bounds a resource's lifetime
//! to an active transaction. The encapsulated resource is accessible transparently
//! via [`Deref`] and [`DerefMut`] while the transaction remains active.
//!
//! # Failure Semantics
//! - Successful completion: Calling [`Protected::commit`] disarms the guard and
//!   returns the inner resource by value.
//! - Failure / Panic / Early exit: If execution leaves the scope without calling
//!   `commit()`, the guard triggers its rollback closure and drops the resource,
//!   preventing stale or corrupted state access.

use super::SnubberGuard;
use core::ops::{Deref, DerefMut};

/// Poison-tolerant mutex access bound to a node ledger (`std` only).
///
/// Wraps the canonical recovery idiom used across `TrackedGuard`
/// settlement paths: on lock poisoning recovers via `into_inner()`
/// and records one `poison_recoveries` tick on `node` so the fault
/// stays visible in telemetry instead of panicking the caller.
///
/// The calling thread MUST NOT hold the poisoned value: recovery
/// extracts the inner guard and continues. Node slot bounds are
/// re-clamped by the caller after recovery where slot counts matter.
#[cfg(feature = "std")]
pub trait SafeLockExt<T> {
    /// Lock or recover: never panics on poisoned mutex.
    fn safe_lock<'a>(&'a self, node: &super::SnubberNode) -> std::sync::MutexGuard<'a, T>;
}

#[cfg(feature = "std")]
impl<T> SafeLockExt<T> for std::sync::Mutex<T> {
    #[inline]
    fn safe_lock<'a>(&'a self, node: &super::SnubberNode) -> std::sync::MutexGuard<'a, T> {
        node.lock_poison_tolerant(self)
    }
}

/// Resource bound to a transactional rollback guard.
///
/// Wraps an underlying value `T` alongside an active [`SnubberGuard`].
/// Transparent access to `T` is provided via [`Deref`] and [`DerefMut`].
/// Extracting `T` by value requires explicit completion via [`commit`](Self::commit).
#[must_use = "Protected resource must be consumed via commit() to retrieve the inner value, or dropped/aborted to trigger rollback"]
pub struct Protected<T, F: FnOnce()> {
    guard: Option<SnubberGuard<F>>,
    resource: Option<T>,
}

impl<T, F: FnOnce()> Protected<T, F> {
    /// Binds a resource to an armed transactional guard.
    #[inline]
    pub fn new(resource: T, guard: SnubberGuard<F>) -> Self {
        Self {
            guard: Some(guard),
            resource: Some(resource),
        }
    }

    /// Access the underlying resource (read-only).
    #[inline]
    #[must_use]
    pub fn get(&self) -> &T {
        // INVARIANT: Guaranteed Some by Protected::new(); only consumed by commit() or Drop.
        self.resource
            .as_ref()
            .expect("protected resource invariant")
    }

    /// Access the underlying resource (read-write).
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self) -> &mut T {
        // INVARIANT: Guaranteed Some by Protected::new(); only consumed by commit() or Drop.
        self.resource
            .as_mut()
            .expect("protected resource invariant")
    }

    /// Disarms the guard and extracts the inner resource upon clean completion.
    ///
    /// This is the sole method to reclaim ownership of `T`.
    #[inline]
    pub fn commit(mut self) -> T {
        if let Some(guard) = self.guard.take() {
            guard.commit();
        }
        // INVARIANT: Guaranteed Some by Protected::new(); only consumed by commit() or Drop.
        self.resource.take().expect("protected resource invariant")
    }

    /// Explicitly aborts the transaction, triggers rollback, and discards the resource.
    ///
    /// Preserves the authentic transaction header from the underlying guard.
    #[inline]
    pub fn abort(mut self, reason: &'static str) -> super::SnubberEvent<()> {
        // Rollback FIRST, resource second — same order as `Drop`. A rollback
        // closure is allowed to read the protected resource; taking it
        // before the guard ran handed the closure a freed value.
        let event = if let Some(guard) = self.guard.take() {
            guard.abort(reason)
        } else {
            super::SnubberEvent::Aborted {
                header: super::TransactionHeader::new(0, 0),
                reason,
            }
        };
        self.resource.take();
        event
    }
}

impl<T, F: FnOnce()> Deref for Protected<T, F> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.get()
    }
}

impl<T, F: FnOnce()> DerefMut for Protected<T, F> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.get_mut()
    }
}

impl<T, F: FnOnce()> Drop for Protected<T, F> {
    #[inline]
    fn drop(&mut self) {
        // Rollback triggers first while resource memory remains allocated.
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        self.resource.take();
    }
}
