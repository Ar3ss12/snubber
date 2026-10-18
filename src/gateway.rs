//! Single-entry channel gateway: multiple senders, one receiver.
//!
//! Contract: senders enqueue an intent into a bounded channel (~10 ns)
//! and return immediately. The receiver loop dequeues in order and
//! mutates state in exactly one place, inside the admission boundary.
//!
//! Rejection is synchronous and non-blocking: if the channel is full,
//! `GatewaySender::send()` immediately returns `Err(GatewaySendError::Full)`.
//!
//! Inside: ONLY `std::sync::mpsc::sync_channel` + `try_send` (F14:
//! plain `Sender` has NO `try_send`, only `SyncSender` does). Never
//! blocks the sender.
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// Plain gateway counters (saturating, mutex-guarded, never panic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GatewayStats {
    /// `send` calls that landed in the channel.
    pub sent: u64,
    /// `recv` calls that delivered an intent to the receiver.
    pub admitted: u64,
    /// Intents dropped due to a full channel (counted).
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
    /// Channel capacity exceeded. Non-blocking drop.
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
}

/// Receiver endpoint: NOT `Clone` — one receiver owns the channel. Runs the consumer loop.
#[derive(Debug)]
pub struct GatewayHost<T> {
    rx: Receiver<T>,
    books: Arc<Mutex<GatewayBooks>>,
}

/// Build a channel gateway.
/// `capacity == 0` saturates to 1 (rendezvous channel, never panics).
#[must_use]
pub fn gateway<T>(capacity: usize) -> (GatewaySender<T>, GatewayHost<T>) {
    let (tx, rx) = sync_channel(capacity.max(1));
    let books = Arc::new(Mutex::new(GatewayBooks::default()));
    (
        GatewaySender {
            tx,
            books: Arc::clone(&books),
        },
        GatewayHost { rx, books },
    )
}

impl<T> GatewaySender<T> {
    /// Enqueue one intent. Never blocks, never panics.
    ///
    /// Returns `Err(GatewaySendError::Full)` when the channel capacity is saturated.
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
