//! Latest-value channel: a producer that never waits meets a consumer that
//! never lags.
//!
//! The slot holds **one** value (queue length `0` or `1`). A publish into an
//! occupied slot replaces the unsampled value and bumps the `conflated`
//! counter; the consumer's next [`ConflateReceiver::take_latest`] sees only
//! the newest state. This is the right topology for UI frames, telemetry
//! dashboards and progress views: stale snapshots are not data, they are
//! work the consumer would do for nothing.
//!
//! Backed by the [`TaskConduit`](crate::TaskConduit) draft table, so the
//! coalescing counter needs no extra allocation and no `Clone` bound on `T`.
//! Non-blocking on both ends: the consumer polls at its own cadence
//! (typically the display refresh), the producer only ever contends for the
//! short slot mutex.

use crate::conduit::{TaskConduit, TaskHandle};

/// Producer endpoint: cheap `Clone` (shares the slot), never blocks on the
/// consumer. One per producer thread.
pub struct ConflateSender<T> {
    conduit: TaskConduit<(), T>,
}

/// Consumer endpoint: owns the sampling cadence, takes the newest value.
pub struct ConflateReceiver<T> {
    handle: TaskHandle<(), T>,
}

impl<T> ConflateSender<T> {
    /// Create a connected latest-value pair.
    #[must_use]
    pub fn pair() -> (Self, ConflateReceiver<T>) {
        let (conduit, handle) = TaskConduit::pair(());
        (Self { conduit }, ConflateReceiver { handle })
    }

    /// Publish a value, replacing any unsampled predecessor.
    ///
    /// Returns `true` when an older value was coalesced; `false` when the
    /// slot was empty. Never blocks on the consumer — only on the short
    /// slot mutex.
    pub fn publish(&self, value: T) -> bool {
        self.conduit.publish_draft(value)
    }

    /// Values overwritten before the consumer sampled them.
    #[must_use]
    pub fn conflated(&self) -> u64 {
        self.conduit.conflated_drafts()
    }
}

impl<T> Clone for ConflateSender<T> {
    /// Second view of the same slot: `Arc` refcount bump, no payload clone.
    #[inline]
    fn clone(&self) -> Self {
        Self {
            conduit: self.conduit.clone(),
        }
    }
}

impl<T> ConflateReceiver<T> {
    /// Take the newest value, leaving the slot empty.
    ///
    /// `None` means no publish since the previous take — never "lagging".
    pub fn take_latest(&self) -> Option<T> {
        self.handle.take_draft()
    }

    /// Values overwritten before this receiver sampled them.
    #[must_use]
    pub fn conflated(&self) -> u64 {
        self.handle.conflated_drafts()
    }
}
