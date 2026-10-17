//! Single Entrance Gateway (Phase 4): many doors, ONE host.
//!
//! Law of the single checkpoint: doors never touch worker state.
//! They `send(cmd)` an intent into a bounded box (~10 ns) and return.
//! The host (one loop, one thread) `recv()`s intents in order and
//! mutates state in exactly one place, under the snubber membrane.
//!
//! Inside: ONLY `std::sync::mpsc::sync_channel` + `try_send` (F14:
//! plain `Sender` has NO `try_send`, only `SyncSender` does). Never
//! blocks the door. Reuses `OverflowPolicy`, no second enum (R1).
use super::OverflowPolicy;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
/// Plain gateway counters (saturating, mutex-guarded, never panic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GatewayStats {
    /// `send` calls that landed in the box.
    pub sent: u64,
    /// `recv` calls that delivered an intent to the host.
    pub admitted: u64,
    /// Intents dropped at a full box (newcomer or stalest, counted).
    pub shed: u64,
}
/// Internal books behind one short mutex (node books untouched).
#[derive(Debug, Default)]
struct GatewayBooks {
    sent: u64,
    admitted: u64,
    shed: u64,
}
/// Why a `send` refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GatewaySendError {
    /// Box full under `RejectNew`: newcomer dropped, counted as shed.
    Full,
    /// Host hung up (`recv` side dropped): dead house.
    Disconnected,
}
/// Why a `recv` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GatewayRecvError {
    /// All doors dropped and the box drained: host shift over.
    Disconnected,
}
/// `send` side: cheap `Clone` (shares box + books). One per door.
#[derive(Debug)]
pub struct GatewaySender<T> {
    tx: SyncSender<T>,
    books: Arc<Mutex<GatewayBooks>>,
    overflow: OverflowPolicy,
}
/// `recv` side: NOT `Clone` (one host, one house). Host loop owns it.
#[derive(Debug)]
pub struct GatewayHost<T> {
    rx: Receiver<T>,
    books: Arc<Mutex<GatewayBooks>>,
}
/// Build the checkpoint: bounded box + books + `RejectNew` rule.
/// `capacity == 0` saturates to 1 (rendezvous box, never panics).
#[must_use]
pub fn gateway<T>(capacity: usize) -> (GatewaySender<T>, GatewayHost<T>) {
    gateway_with_overflow(capacity, OverflowPolicy::RejectNew)
}
/// Build with an explicit overflow rule (UI doors: `EvictOldest`).
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
    /// Switch the overflow rule live (doors adapt without rebuild).
    pub fn set_overflow(&mut self, overflow: OverflowPolicy) {
        self.overflow = overflow;
    }
    /// Current overflow rule.
    #[must_use]
    pub fn overflow(&self) -> OverflowPolicy {
        self.overflow
    }
    /// Offer one intent. Never blocks, never panics.
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
    /// Snapshot the books (`Copy` out, poison-safe default).
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
    /// Cheap clone: shares the box + books (one more door).
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
    /// Take the next intent (blocks: the host parks here, doors never).
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
    /// Non-blocking take (`None` = box empty right now).
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
    /// Land freshest under `EvictOldest` (HOST side owns the dequeue).
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
    /// Snapshot the books.
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
