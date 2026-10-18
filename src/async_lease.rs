//! Bounded lease channel: honest `try_send` on `SyncSender`, zero deps.
//!
//! `std::sync::mpsc::Sender` (unbounded) has NO `try_send` — only
//! [`std::sync::mpsc::SyncSender`] (from `sync_channel`) does. A bounded
//! queue caps memory consumption during peak load; overflows are recorded
//! as `BufferOverflow`.
//!
//! Used by background cleanup tasks: `Drop` fires `try_send` (never blocks), the
//! background task drains and runs the heavy rollback.

use super::policy::FaultKind;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};

/// One lease event: what died at which stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeaseEvent {
    /// Correlation id of the dead transaction.
    pub task_id: u64,
    /// Pipeline stage reached (`set_stage`), reaper undoes up to here.
    pub stage: u32,
    /// What kind of death (usually `ImplicitDrop` / `ProcessDied`).
    pub kind: FaultKind,
}

/// Bounded reaper mailbox: `try_send` never blocks, overflow is counted.
///
/// `LeaseChannel::pair(cap)` builds the pair; `LeaseChannel::fire` sends
/// from `Drop` without blocking. The struct itself is only the namespace
/// (Rust has no static-only impl without a type) — never instantiated.
#[derive(Debug)]
pub struct LeaseChannel {
    _sealed: (),
}

impl LeaseChannel {
    /// Build with `cap` slots (1024 recommended for a storm fence).
    #[must_use]
    pub fn pair(cap: usize) -> (SyncSender<LeaseEvent>, Receiver<LeaseEvent>) {
        let (tx, rx) = sync_channel(cap.max(1));
        (tx, rx)
    }

    /// Fire from `Drop`: never blocks; on `Full` count `BufferOverflow`,
    /// on `Disconnected` count `ChannelDisconnected`. Returns `true` if
    /// the reaper will see it.
    #[must_use = "LeaseChannel::fire reports whether the reaper will see the event — count the false leg, never drop it"]
    pub fn fire(
        tx: &SyncSender<LeaseEvent>,
        ev: LeaseEvent,
        dropped_full: &mut u64,
        dropped_gone: &mut u64,
    ) -> bool {
        match tx.try_send(ev) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                *dropped_full = dropped_full.saturating_add(1);
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                *dropped_gone = dropped_gone.saturating_add(1);
                false
            }
        }
    }
}

/// RAII lease tied to a bounded [`SyncSender<LeaseEvent>`]: on `Drop` without
/// `commit()`, fires `try_send` carrying the last `stage` reached (`set_stage`).
///
/// Bounded by construction: `SyncSender` has a hard capacity ceiling (no memory
/// leaks during load spikes). `try_send` in `Drop` never blocks the worker thread.
#[must_use = "an uncommitted AsyncLease immediately sends an abort event on drop"]
#[derive(Debug)]
pub struct AsyncLease {
    tx: SyncSender<LeaseEvent>,
    task_id: u64,
    stage: u32,
    committed: bool,
}

impl AsyncLease {
    /// Arm a lease for `task_id` wired to `tx`.
    pub const fn new(tx: SyncSender<LeaseEvent>, task_id: u64) -> Self {
        Self {
            tx,
            task_id,
            stage: 0,
            committed: false,
        }
    }

    /// Mark the pipeline stage reached (survives `select!` cancel).
    #[inline]
    pub fn set_stage(&mut self, stage: u32) {
        self.stage = stage;
    }

    /// Current pipeline stage.
    #[inline]
    #[must_use]
    pub const fn stage(&self) -> u32 {
        self.stage
    }

    /// Disarm the lease: transaction succeeded, reaper has nothing to undo.
    #[inline]
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for AsyncLease {
    fn drop(&mut self) {
        if !self.committed {
            let ev = LeaseEvent {
                task_id: self.task_id,
                stage: self.stage,
                kind: FaultKind::ImplicitDrop,
            };
            let _ = self.tx.try_send(ev);
        }
    }
}
