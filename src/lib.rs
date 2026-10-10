//! `snubber` — ultra-light transactional damper and RAII boundary shim.
//!
//! Three floors, zero bloat:
//!
//! * **Floor 1 (core-only, v0.1):** [`SnubberGuard`] (Drop-guaranteed
//!   rollback) + [`EpochGate`] (monotonic late-packet filter).
//! * **Floor 2 (`std`, v0.2):** [`SnubberNode`] — bounded bulkhead,
//!   circuit breaker, flight-recorder stats, `#[track_caller]` fault
//!   coordinates and a capped console reporter.
//! * **Floor 3 (`alloc`, in `default` since v0.3):**
//!   [`crate::ErasedGuard`] (nameable struct-field guard),
//!   [`crate::ClosurePolicy`] (capturing self-healing commands) and
//!   [`crate::FaultDetail`] (static-or-owned breach reasons —
//!   `Static` is zero-cost, `Owned` carries live values like
//!   `interval=359999640000`; no Clone-on-write semantics by design).
//!
//! Core stays dependency-free and `unsafe`-free; the node adds only
//! `std::sync` (shared recorder), no runtimes, no HTTP, no async.
//!
//! # Minimal example
//!
//! ```rust
//! use snubber::SnubberGuard;
//!
//! fn set_hardware_mode(mode: u8) { /* ... */ }
//!
//! let guard = SnubberGuard::new(7, 42, || set_hardware_mode(0));
//! let _ack = guard.ack::<()>();
//! // ... do fallible work, emit `guard.progress(payload)` ...
//! let committed = guard.commit(); // rollback disarmed here
//! assert!(committed.is_terminal());
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Universal invariant assertion macro for semantic customs gates.
///
/// Evaluates a boolean expression; if false, returns early with `Err(reason)`.
///
/// # Examples
/// ```rust
/// use snubber::snub_check;
///
/// fn check_order(amount: i64) -> Result<(), &'static str> {
///     snub_check!(amount > 0, "order amount must be positive");
///     Ok(())
/// }
/// assert!(check_order(100).is_ok());
/// assert!(check_order(-5).is_err());
/// ```
#[macro_export]
macro_rules! snub_check {
    ($cond:expr, $reason:expr) => {
        if !($cond) {
            return Err($reason);
        }
    };
}

#[cfg(feature = "std")]
pub mod node;

#[cfg(feature = "std")]
pub mod supervisor;

#[cfg(feature = "std")]
pub mod async_entry;

#[cfg(feature = "std")]
pub mod async_lease;

#[cfg(feature = "std")]
pub mod async_pace;

#[cfg(feature = "std")]
pub mod async_settle;

#[cfg(feature = "std")]
pub mod async_gate;

#[cfg(feature = "std")]
pub mod intercom;

pub mod policy;

pub mod contract;

pub mod watchdog;

#[cfg(feature = "alloc")]
pub mod closure;

pub use policy::{
    Backpressure, BackpressurePolicy, FaultAction, FaultDetail, FaultKind, FaultPolicy, FrictionFn,
    LogLevel, MetricSample, PressureFn,
};

#[cfg(feature = "alloc")]
pub use closure::{ClosurePolicy, ErasedGuard};

#[cfg(feature = "std")]
pub use node::{
    BreakerState, EnterError, FaultHook, FaultRecord, GuardSnapshot, OverflowPolicy, SnubberError,
    SnubberNode, SnubberNodeBuilder, SnubberStats, StorageKind, TimeoutCheck, TrackedGuard,
    DEFAULT_POLICIES,
};

#[cfg(feature = "std")]
pub use supervisor::{
    DeathCause, GenerationClock, PipeDrain, ProcessToken, Supervisor, TokenRollback, OP_COMMIT,
    OP_HEARTBEAT,
};

#[cfg(feature = "std")]
pub use async_entry::EnterFuture;

#[cfg(feature = "std")]
pub use async_lease::{AsyncLease, LeaseChannel, LeaseEvent};

#[cfg(feature = "std")]
pub use async_pace::enter_paced_async;

#[cfg(feature = "std")]
pub use async_settle::settle_async;

#[cfg(feature = "std")]
pub use async_gate::{
    backpressure_of, classify, retry_after_ms, AsyncGate, AsyncSleeper, GateError, Refusal,
};

#[cfg(feature = "std")]
pub use intercom::{Directive, ResumeFuture, TaskConduit, TaskHandle};

use core::fmt;
use core::mem;

/// Monotonic session/action correlation header attached to every event.
///
/// Two-dimensional coordinates against Cold-Boot Split-Brain:
/// * `generation` — client lifetime/session id (PID, boot timestamp,
///   seed+1…). A rebooted client starts a NEW generation at `epoch = 1`;
///   the gate re-handshakes instead of wedging on `last_seen = 1420`.
/// * `epoch` — monotonic tick INSIDE the current generation. Any packet
///   with `epoch <= last_seen` in the SAME generation is late.
/// * `task_id` — correlation id of the session/action (the "cynical"
///   check: a reply for task 9 never settles task 7).
///
/// Wire cost: one extra `u64` (24 bytes total, still `Copy`, zero heap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionHeader {
    /// Client lifetime/session id for reboot re-handshake.
    pub generation: u64,
    /// Monotonic tick inside the current generation.
    pub epoch: u64,
    /// Correlation id of the session/action.
    pub task_id: u64,
}

impl TransactionHeader {
    /// Build a header explicitly (preferred at trust boundaries: the caller
    /// stamps all fields, nothing is inferred).
    #[inline]
    #[must_use]
    pub const fn new(epoch: u64, task_id: u64) -> Self {
        Self {
            generation: 0,
            epoch,
            task_id,
        }
    }

    /// Build a header with an explicit generation (reboot-aware clients).
    #[inline]
    #[must_use]
    pub const fn with_generation(generation: u64, epoch: u64, task_id: u64) -> Self {
        Self {
            generation,
            epoch,
            task_id,
        }
    }
}

impl fmt::Display for TransactionHeader {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gen={} epoch={} task={}",
            self.generation, self.epoch, self.task_id
        )
    }
}
/// Strict lifecycle of one transaction on an external bus / IPC channel.
///
/// * `Ack` — intent accepted, resources locked.
/// * `Progress` — live telemetry / editable draft in flight.
/// * `Committed` — action closed successfully (terminal).
/// * `Aborted` — emergency rollback (terminal) with a static reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnubberEvent<T> {
    /// Intent accepted, resources locked.
    Ack {
        /// Header of the transaction being acknowledged.
        header: TransactionHeader,
    },
    /// Live telemetry payload in flight (non-terminal).
    Progress {
        /// Header this payload belongs to.
        header: TransactionHeader,
        /// Telemetry / draft payload.
        payload: T,
    },
    /// Action closed successfully (terminal).
    Committed {
        /// Header of the committed transaction.
        header: TransactionHeader,
    },
    /// Emergency rollback (terminal).
    Aborted {
        /// Header of the aborted transaction.
        header: TransactionHeader,
        /// Static reason code (`'static` keeps the core `alloc`-free).
        reason: &'static str,
    },
}

impl<T> SnubberEvent<T> {
    /// The header this event belongs to (all variants carry it).
    #[inline]
    #[must_use]
    pub const fn header(&self) -> TransactionHeader {
        match *self {
            Self::Ack { header }
            | Self::Progress { header, .. }
            | Self::Committed { header }
            | Self::Aborted { header, .. } => header,
        }
    }

    /// `true` for `Committed` / `Aborted` (nothing may follow them).
    #[inline]
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Committed { .. } | Self::Aborted { .. })
    }

    /// Map the `Progress` payload, passing the other three variants through.
    #[inline]
    #[must_use]
    pub fn map_payload<U, F: FnOnce(T) -> U>(self, f: F) -> SnubberEvent<U> {
        match self {
            Self::Ack { header } => SnubberEvent::Ack { header },
            Self::Progress { header, payload } => SnubberEvent::Progress {
                header,
                payload: f(payload),
            },
            Self::Committed { header } => SnubberEvent::Committed { header },
            Self::Aborted { header, reason } => SnubberEvent::Aborted { header, reason },
        }
    }
}

impl<T> fmt::Display for SnubberEvent<T> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ack { header } => write!(f, "ack({header})"),
            Self::Progress { header, .. } => write!(f, "progress({header})"),
            Self::Committed { header } => write!(f, "committed({header})"),
            Self::Aborted { header, reason } => {
                write!(f, "aborted({header} reason={reason})")
            }
        }
    }
}

/// Monotonic late-packet filter with reboot re-handshake.
///
/// Two-dimensional coordinates against Cold-Boot Split-Brain:
/// * NEW generation (`header.generation > current`) — the client rebooted
///   (or a fresh client appeared). The gate re-handshakes on the fly:
///   adopts the generation, resets `last_seen` to this epoch, returns
///   `true`. No backend restart, no manual epoch reset.
/// * OLD generation (`header.generation < current`) — a zombie packet from
///   a dead client surfacing from a queue. Rejected flat, state untouched.
/// * SAME generation — classic monotonic check: `epoch > last_seen`.
///
/// Starts at `(0, 0)`; generation `0` is the boot session, epoch `0`
/// inside any generation is never fresh. Cost: one extra `u64`
/// comparison per packet (~2 CPU ticks), zero heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpochGate {
    current_generation: u64,
    last_seen: u64,
}

impl EpochGate {
    /// Closed gate: nothing observed yet.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current_generation: 0,
            last_seen: 0,
        }
    }

    /// Observe one header. `true` + advance on fresh epochs (or on a
    /// newer generation — re-handshake), `false` + keep state on stale
    /// or zombie packets.
    #[inline]
    pub fn observe(&mut self, header: &TransactionHeader) -> bool {
        if header.generation > self.current_generation {
            self.current_generation = header.generation;
            self.last_seen = header.epoch;
            return header.epoch > 0;
        }
        if header.generation < self.current_generation {
            return false;
        }
        if header.epoch > self.last_seen {
            self.last_seen = header.epoch;
            true
        } else {
            false
        }
    }

    /// Highest epoch accepted so far in the current generation.
    #[inline]
    #[must_use]
    pub const fn last_seen(&self) -> u64 {
        self.last_seen
    }

    /// Currently adopted generation (0 = boot session).
    #[inline]
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.current_generation
    }
}
/// RAII transaction guard: owns exactly one rollback.
///
/// Drop semantics (the whole point of the crate):
/// if [`SnubberGuard::commit`] (or [`SnubberGuard::abort`]) did not consume
/// the guard, `Drop` runs the rollback closure. Panic unwinding, `?`
/// propagation, early `return`, a dropped channel — all funnel through `Drop`,
/// so a stuck hardware/backend state is structurally impossible as long as
/// the rollback closure restores it.
///
/// Rules for the rollback closure (contract — READ BEFORE USE):
///
///   1. Non-panicking and lock-free on real-time paths (same contract as
///      any hot-path destructor). Under `std` a panicking rollback is
///      swallowed by `catch_unwind` (no `abort`); without `std` a rollback
///      panic during unwind aborts the process — that is a Rust rule, not a
///      `snubber` rule.
///
///   2. Idempotent: `f(f(x)) == f(x)`. Release/zeroize/mute twice is safe;
///      send-money-twice is NOT — non-idempotent work belongs ONLY in the
///      `commit()` branch, never in a rollback.
///
///   3. Time-bounded: atomics, `SetEvent`, non-blocking sends. A rollback
///      that hangs in a driver turns the process into a zombie (see
///      `docs/FUTURE_TRAPS.md` Trap 2) — no crate can preempt kernel.
///
///   4. Lock-free vs the node: never call back into the same `SnubberNode`
///      (open-call rule — the node drops its lock first, but re-entrant
///      `enter` from inside a rollback is a deadlock smell).
///
///   5. Ground your own shared memory: the node's lock guards ONLY node
///      books. A rollback runs outside it (Floor-1 guards own no lock at
///      all), so publishing to foreign state needs `Release` and observing
///      it needs `Acquire` — or your own lock. `Relaxed` on both ends may
///      read rot on another core.
///
/// The guard itself is `core`-only and `unsafe`-free; the `Drop` runs on
/// the language stack, independent of any node bookkeeping.
#[must_use = "a SnubberGuard that is never committed rolls back on drop"]
pub struct SnubberGuard<F: FnOnce()> {
    header: TransactionHeader,
    rollback: Option<F>,
}

impl<F: FnOnce()> SnubberGuard<F> {
    /// Arm a guard: stamp the header, hold the rollback.
    #[inline]
    pub const fn new(epoch: u64, task_id: u64, rollback: F) -> Self {
        Self {
            header: TransactionHeader::new(epoch, task_id),
            rollback: Some(rollback),
        }
    }

    /// The header stamped at arming time.
    #[inline]
    #[must_use]
    pub const fn header(&self) -> TransactionHeader {
        self.header
    }

    /// Emit the `Ack` event for this transaction (locks resources).
    #[inline]
    #[must_use]
    pub fn ack<T>(&self) -> SnubberEvent<T> {
        SnubberEvent::Ack {
            header: self.header,
        }
    }

    /// Emit a `Progress` event carrying a live payload.
    #[inline]
    #[must_use]
    pub fn progress<T>(&self, payload: T) -> SnubberEvent<T> {
        SnubberEvent::Progress {
            header: self.header,
            payload,
        }
    }

    /// Success path: disarm the destructor, report `Committed`.
    ///
    /// The rollback is taken out and `self` is forgotten so `Drop` never
    /// runs for a committed transaction.
    #[inline]
    pub fn commit(mut self) -> SnubberEvent<()> {
        let _ = self.rollback.take();
        let event = SnubberEvent::Committed {
            header: self.header,
        };
        mem::forget(self);
        event
    }

    /// Explicit failure path: run the rollback NOW, report `Aborted`.
    ///
    /// Use it when the fallible work returned an error value (as opposed to
    /// panicking / early-dropping, where `Drop` covers you implicitly).
    #[inline]
    pub fn abort(mut self, reason: &'static str) -> SnubberEvent<()> {
        if let Some(rollback) = self.rollback.take() {
            rollback();
        }
        let event = SnubberEvent::Aborted {
            header: self.header,
            reason,
        };
        mem::forget(self);
        event
    }
}

impl<F: FnOnce()> Drop for SnubberGuard<F> {
    #[inline]
    fn drop(&mut self) {
        if let Some(rollback) = self.rollback.take() {
            // Bullet 1 — double-panic abort: if the worker is already
            // unwinding AND the idiot's rollback panics too, Rust aborts
            // the whole process. Under `std` we swallow the second panic;
            // without `std` there is no catcher — documented, not hidden.
            #[cfg(feature = "std")]
            {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(rollback));
            }
            #[cfg(not(feature = "std"))]
            {
                rollback();
            }
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};

    static FLAG: AtomicBool = AtomicBool::new(false);

    #[test]
    fn header_display_names_all_fields() {
        let text = TransactionHeader::new(3, 9).to_string();
        assert_eq!(text, "gen=0 epoch=3 task=9");
        let gen = TransactionHeader::with_generation(2, 1, 7).to_string();
        assert_eq!(gen, "gen=2 epoch=1 task=7");
    }

    #[test]
    fn terminal_flags_match_lifecycle() {
        let h = TransactionHeader::new(1, 1);
        assert!(!SnubberEvent::<()>::Ack { header: h }.is_terminal());
        assert!(!SnubberEvent::Progress {
            header: h,
            payload: 1
        }
        .is_terminal());
        assert!(SnubberEvent::<()>::Committed { header: h }.is_terminal());
        assert!(SnubberEvent::<()>::Aborted {
            header: h,
            reason: "x"
        }
        .is_terminal());
    }

    #[test]
    fn epoch_zero_is_never_fresh() {
        let mut gate = EpochGate::new();
        assert!(!gate.observe(&TransactionHeader::new(0, 1)));
        assert_eq!(gate.last_seen(), 0);
        assert_eq!(gate.generation(), 0);
    }

    #[test]
    fn generation_rehandshake_heals_split_brain() {
        // Client lives: gen 0, epochs 1..=1420.
        let mut gate = EpochGate::new();
        assert!(gate.observe(&TransactionHeader::new(1420, 1)));
        // Client crashes, reboots as generation 1 at epoch 1.
        let reborn = TransactionHeader::with_generation(1, 1, 1);
        assert!(
            gate.observe(&reborn),
            "new generation must re-handshake, not wedge"
        );
        assert_eq!(gate.generation(), 1);
        assert_eq!(gate.last_seen(), 1);
        // Zombie packet from the dead generation surfaces from a queue.
        let zombie = TransactionHeader::with_generation(0, 1421, 1);
        assert!(!gate.observe(&zombie), "old generation is a phantom");
        assert_eq!(gate.last_seen(), 1, "zombie must not move state");
        // Same-generation duplicates still cut.
        assert!(!gate.observe(&reborn), "replay inside generation cut");
        assert!(gate.observe(&TransactionHeader::with_generation(1, 2, 1)));
    }

    #[test]
    #[cfg(feature = "std")]
    fn panicking_rollback_does_not_abort_process() {
        // The idiot's rollback: panics inside Drop while the worker is
        // ALREADY unwinding. Without catch_unwind this is instant abort.
        let probe = std::panic::AssertUnwindSafe(|| {
            let _g = SnubberGuard::new(1, 1, || panic!("rollback idiot"));
            panic!("worker failure");
        });
        let caught = std::panic::catch_unwind(probe).expect_err("must unwind");
        let msg = caught
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| caught.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string>");
        assert!(
            msg.contains("worker failure"),
            "outer panic survives: {msg}"
        );
        // Reaching this line PROVES no abort happened.
    }

    #[test]
    fn drop_without_commit_runs_rollback() {
        FLAG.store(false, Ordering::SeqCst);
        {
            let _g = SnubberGuard::new(1, 1, || FLAG.store(true, Ordering::SeqCst));
        }
        assert!(FLAG.load(Ordering::SeqCst));
    }
}
