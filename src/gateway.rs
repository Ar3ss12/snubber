//! Single-entry channel gateway: multiple senders, one receiver.
//!
//! Contract: senders enqueue an intent into a bounded channel (~10 ns)
//! and return immediately. The receiver loop dequeues in order and
//! mutates state in exactly one place, inside the admission boundary.
//!
//! Overflow policy split by design (structural constraint, not a bug):
//! * `RejectNew` — `GatewaySender::send()` handles it: returns `Full`.
//! * `EvictOldest` — requires draining from the receiver side; senders
//!   have no access to the `Receiver`. Use `GatewayHost::send_fresh()`
//!   when the queue is full and eviction is needed.
//!
//! Inside: ONLY `std::sync::mpsc::sync_channel` + `try_send` (F14:
//! plain `Sender` has NO `try_send`, only `SyncSender` does). Never
//! blocks the sender. Reuses `OverflowPolicy`, no second enum (R1).
use super::OverflowPolicy;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// Plain gateway counters (saturating, mutex-guarded, never panic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GatewayStats {
    /// `send` calls that landed in the channel.
    pub sent: u64,
    /// `recv` calls that delivered an intent to the receiver.
    pub admitted: u64,
    /// Intents dropped at a full channel (rejected or evicted, counted).
    pub shed: u64,
}

/// Internal counters behind one short mutex (node counters untouched).
#[derive(Debug, Default)]
struct GatewayBooks {
    sent: u64,
    admitted: u64,
    shed: u64,
}

/// Why a `send` call was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GatewaySendError {
    /// Channel capacity exceeded. Under `RejectNew` this is final.
    /// Under `EvictOldest`, `GatewaySender::send()` cannot evict (no
    /// access to the receiver side) — call `GatewayHost::send_fresh()`
    /// instead, which dequeues the oldest entry and retries.
    Full,
    /// Receiver dropped; channel is broken, intent cannot be delivered.
    Disconnected,
}

/// Why a blocking `recv` returned without a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GatewayRecvError {
    /// All senders dropped and the channel queue is empty.
    Disconnected,
}

/// Sender endpoint: cheap `Clone` (shares channel + counters). One per producer thread.
#[derive(Debug)]
pub struct GatewaySender<T> {
    tx: SyncSender<T>,
    books: Arc<Mutex<GatewayBooks>>,
    overflow: OverflowPolicy,
}

/// Receiver endpoint: NOT `Clone` — one receiver owns the channel. Runs the consumer loop.
#[derive(Debug)]
pub struct GatewayHost<T> {
    rx: Receiver<T>,
    books: Arc<Mutex<GatewayBooks>>,
}

/// Build a channel gateway with `RejectNew` overflow policy.
/// `capacity == 0` saturates to 1 (rendezvous channel, never panics).
#[must_use]
pub fn gateway<T>(capacity: usize) -> (GatewaySender<T>, GatewayHost<T>) {
    gateway_with_overflow(capacity, OverflowPolicy::RejectNew)
}

/// Build a channel gateway with an explicit overflow policy.
#[must_use]
pub fn gateway_with_overflow<T>(
    capacity: usize,
    overflow: OverflowPolicy,
) -> (GatewaySender<T>, GatewayHost<T>) {
    let (tx, rx) = sync_channel(capacity.max(1));
    let books = Arc::new(Mutex::new(GatewayBooks::default()));
    (
        GatewaySender {
            tx,
            books: Arc::clone(&books),
            overflow,
        },
        GatewayHost { rx, books },
    )
}

impl<T> GatewaySender<T> {
    /// Update the overflow policy on this sender.
    pub fn set_overflow(&mut self, overflow: OverflowPolicy) {
        self.overflow = overflow;
    }

    /// Current overflow policy on this sender.
    #[must_use]
    pub fn overflow(&self) -> OverflowPolicy {
        self.overflow
    }

    /// Enqueue one intent. Never blocks, never panics.
    ///
    /// Returns `Err(Full)` when the channel is at capacity. If the policy
    /// is `EvictOldest`, the caller must use `GatewayHost::send_fresh()`
    /// to evict the oldest entry — the sender has no access to the receiver.
    pub fn send(&self, cmd: T) -> Result<(), GatewaySendError> {
        match self.tx.try_send(cmd) {
            Ok(()) => {
                if let Ok(mut b) = self.books.lock() {
                    b.sent = b.sent.saturating_add(1);
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                if let Ok(mut b) = self.books.lock() {
                    b.shed = b.shed.saturating_add(1);
                }
                Err(GatewaySendError::Full)
            }
            Err(TrySendError::Disconnected(_)) => Err(GatewaySendError::Disconnected),
        }
    }

    /// Snapshot the counters (`Copy` out, poison-safe default).
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        self.books
            .lock()
            .map_or(GatewayStats::default(), |b| GatewayStats {
                sent: b.sent,
                admitted: b.admitted,
                shed: b.shed,
            })
    }
}

impl<T> Clone for GatewaySender<T> {
    /// Arc refcount bump only — no clone of channel data or counters.
    #[inline]
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            books: Arc::clone(&self.books),
            overflow: self.overflow,
        }
    }
}

impl<T> GatewayHost<T> {
    /// Blocking dequeue: receiver parks here until an intent arrives or all senders drop.
    pub fn recv(&self) -> Result<T, GatewayRecvError> {
        match self.rx.recv() {
            Ok(cmd) => {
                if let Ok(mut b) = self.books.lock() {
                    b.admitted = b.admitted.saturating_add(1);
                }
                Ok(cmd)
            }
            Err(_) => Err(GatewayRecvError::Disconnected),
        }
    }

    /// Non-blocking dequeue. Returns `None` if the channel is empty right now.
    #[must_use]
    pub fn try_recv(&self) -> Option<T> {
        match self.rx.try_recv() {
            Ok(cmd) => {
                if let Ok(mut b) = self.books.lock() {
                    b.admitted = b.admitted.saturating_add(1);
                }
                Some(cmd)
            }
            Err(_) => None,
        }
    }

    /// Enqueue under `EvictOldest`: dequeues the oldest entry to free a slot, then retries.
    ///
    /// Only the receiver can evict because eviction requires `Receiver::try_recv()`.
    /// `GatewaySender::send()` returns `Err(Full)` when the queue is full under
    /// `EvictOldest` — that is the signal to call this method instead.
    pub fn send_fresh(&self, tx: &GatewaySender<T>, cmd: T) -> Result<(), GatewaySendError> {
        match tx.tx.try_send(cmd) {
            Ok(()) => {
                if let Ok(mut b) = self.books.lock() {
                    b.sent = b.sent.saturating_add(1);
                }
                Ok(())
            }
            Err(TrySendError::Full(cmd)) => {
                if self.rx.try_recv().is_ok() {
                    if let Ok(mut b) = self.books.lock() {
                        b.shed = b.shed.saturating_add(1);
                    }
                }
                match tx.tx.try_send(cmd) {
                    Ok(()) => {
                        if let Ok(mut b) = self.books.lock() {
                            b.sent = b.sent.saturating_add(1);
                        }
                        Ok(())
                    }
                    Err(TrySendError::Full(_)) => {
                        if let Ok(mut b) = self.books.lock() {
                            b.shed = b.shed.saturating_add(1);
                        }
                        Err(GatewaySendError::Full)
                    }
                    Err(TrySendError::Disconnected(_)) => Err(GatewaySendError::Disconnected),
                }
            }
            Err(TrySendError::Disconnected(_)) => Err(GatewaySendError::Disconnected),
        }
    }

    /// Snapshot the counters.
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        self.books
            .lock()
            .map_or(GatewayStats::default(), |b| GatewayStats {
                sent: b.sent,
                admitted: b.admitted,
                shed: b.shed,
            })
    }
}
