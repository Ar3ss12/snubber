//! `snubber` — transactional RAII guard with bulkhead node and failure-recovery engine.
//!
//! Three layers:
//!
//! * **Layer 1 (core-only, v0.1):** [`SnubberGuard`] (Drop-guaranteed
//!   rollback) + [`EpochGate`] (monotonic late-packet filter).
//! * **Layer 2 (`std`, v0.2):** [`SnubberNode`] — bounded bulkhead,
//!   circuit breaker, flight-recorder stats, `#[track_caller]` fault
//!   coordinates and a capped console reporter.
//! * **Layer 3 (`alloc`, in `default` since v0.3):**
//!   [`crate::ErasedGuard`] (nameable struct-field guard),
//!   [`crate::ClosurePolicy`] (capturing self-healing commands) and
//!   [`crate::FaultDetail`] (static-or-owned breach reasons —
//!   `Static` is zero-cost, `Owned` carries live values like
//!   `interval=359999640000`; no Clone-on-write semantics by design).
//!
//! Core stays dependency-free and `unsafe`-free; the node adds only
//! `std::sync` (shared recorder), no runtimes, no HTTP, no async.
//!
//! # Feature gates
//!
//! `std` and `alloc` are additive. Without `std` the crate is
//! `#![no_std]` (`core` only) and builds for bare-metal targets; `alloc`
//! adds `String`-backed fault details, `ErasedGuard` and `Vec` helpers.
//! The supervisor and pool modules require `std` (OS pipes and reader
//! threads); [`PipeDrain::feed`](crate::PipeDrain::feed) lets embedded
//! and async callers drive the same parser by pushing chunks, and
//! [`ProcessToken::write_to`](crate::ProcessToken::write_to) targets any
//! `Write` sink instead of stdout.
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

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(test)]
extern crate std;

/// Universal invariant assertion macro for semantic admission boundary gates.
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
    ($cond:expr, $reason:expr $(,)?) => {
        if !($cond) {
            return Err($reason);
        }
    };
}

/// Invariant assertion with error MAPPING: `Err(From::from(err))`.
///
/// Use it when the invariant reason must become the caller's error type
/// (`&'static str` → `String`, domain enum, `anyhow::Error`, …) without a
/// hand-written `return Err(...into())` at every gate. The target type is
/// inferred from the function's return type.
///
/// # Examples
/// ```rust
/// use snubber::snub_check_map;
///
/// #[derive(Debug, PartialEq)]
/// struct Refused(&'static str);
///
/// impl From<&'static str> for Refused {
///     fn from(reason: &'static str) -> Self {
///         Self(reason)
///     }
/// }
///
/// fn check_order(amount: i64) -> Result<(), Refused> {
///     snub_check_map!(amount > 0, "order amount must be positive");
///     Ok(())
/// }
/// assert!(check_order(100).is_ok());
/// assert_eq!(check_order(-5), Err(Refused("order amount must be positive")));
/// ```
#[macro_export]
macro_rules! snub_check_map {
    ($cond:expr, $err:expr $(,)?) => {
        if !($cond) {
            return Err(::core::convert::From::from($err));
        }
    };
}

#[cfg(feature = "std")]
pub mod node;

#[cfg(feature = "std")]
pub mod supervisor;

#[cfg(feature = "std")]
pub mod pool;

#[cfg(feature = "std")]
pub mod async_engine;

#[cfg(feature = "std")]
pub mod conduit;

#[cfg(feature = "std")]
pub mod conflate;

#[cfg(feature = "std")]
pub mod channel;

#[cfg(feature = "std")]
pub mod harness;

pub mod clock;

pub mod isr;

pub mod policy;

pub mod contract;

pub mod envelope;

pub mod watchdog;

pub mod diagnostics;

pub mod memory;

pub mod protect;

#[cfg(feature = "alloc")]
pub mod erased;

#[cfg(feature = "alloc")]
pub mod name;

#[cfg(feature = "alloc")]
pub use name::NodeName;

pub use policy::{
    Backpressure, BackpressurePolicy, FaultAction, FaultDetail, FaultKind, FaultPolicy, FrictionFn,
    LogLevel, MetricSample, PressureFn, Priority,
};

pub use clock::{
    classify_stall, classify_stall_ceiling, CpuClock, IdlePark, MonotonicMs, NoWdt, PulseWatch,
    SpinPark, StallKind, WdtSink, WorkerPulse,
};

pub use envelope::{EnvelopeVerdict, ExecutionEnvelope, PayloadVerdict, TimeVerdict};

pub use isr::{
    IsrBridge, IsrPushError, IsrSignal, ISR_RING_CAP, ISR_RING_MASK, OP_EMERGENCY_TRIP, OP_PAUSE,
    OP_PULSE,
};

#[cfg(feature = "alloc")]
pub use erased::{ClosurePolicy, ErasedGuard};

#[cfg(feature = "std")]
pub use node::{
    std_mono_ns, BreakerState, EnterError, ExecutionProfile, FaultHook, FaultRecord, FlightSlot,
    GuardSnapshot, LegalMove, NanoClock, OverflowPolicy, ScopeToken, SnubberError, SnubberNode,
    SnubberNodeBuilder, SnubberStats, SpawnedGuard, StageKind, StageSpan, StaticNode, SystemClock,
    TelemetrySink, TimeoutCheck, TrackedGuard, DEFAULT_POLICIES, PROFILE_HAS_CPU,
    PROFILE_SATURATED, PROFILE_STAGES, PROFILE_STAGE_OVERFLOW,
};

#[cfg(feature = "std")]
pub use diagnostics::{
    diagnostic_line, find_anomalies, print_health, Anomaly, AnomalyKind, AnomalyPolicy, NodeSet,
    SnapshotDiff, SnapshotError, SnapshotV1, SNAPSHOT_CRC_RANGE, SNAPSHOT_LEN, SNAPSHOT_MAGIC,
    SNAPSHOT_VERSION,
};

#[cfg(not(feature = "std"))]
pub use diagnostics::{
    Anomaly, AnomalyKind, AnomalyPolicy, SnapshotDiff, SnapshotError, SnapshotV1,
    SNAPSHOT_CRC_RANGE, SNAPSHOT_LEN, SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
};

pub use diagnostics::{
    crc32_ieee, diagnostic_hash, scan_anomalies_for_snapshot, write_diagnostic_line, write_health,
    write_triage,
};

#[cfg(feature = "alloc")]
pub use diagnostics::find_anomalies_for_snapshot;

pub use memory::{MemoryVerdict, MemoryWatch};

pub use protect::Protected;

#[cfg(feature = "std")]
pub use protect::SafeLockExt;

#[cfg(feature = "std")]
pub use supervisor::{
    now_ms, take_stdin_digest, Backoff, ClockFn, DeathCause, DrainCounters, GenerationClock,
    MemoryPolicy, NoOrphanGuard, OrphanGuard, PipeDrain, ProcessToken, RestartDecision,
    RestartPolicy, RestartTracker, RestartWindow, StateDigest, Supervisor, TokenRollback,
    OP_COMMIT, OP_HEARTBEAT, OP_SNAPSHOT, OP_STATE, STATE_LEN, TOKEN_BODY_LEN, TOKEN_PAYLOAD_LEN,
};

#[cfg(feature = "std")]
pub use pool::{
    namespace_task, task_lane, task_local, LaneSnapshot, PoolConfig, PoolError, PoolLane, PoolReap,
    PoolReport, PoolTotals, ReapClass, WorkerPool, DEFAULT_EOF_GRACE_MS, LANE_SHIFT, MAX_LANES,
    TASK_LOCAL_MASK,
};

#[cfg(feature = "std")]
pub use async_engine::{
    backpressure_of, classify, enter_paced_async, retry_after_ms, settle_async, AsyncGate,
    AsyncLease, EnterFuture, GateError, LeaseChannel, LeaseEvent, ProfiledEnterFuture, Refusal,
};

#[cfg(feature = "std")]
pub use conduit::{Directive, ResumeFuture, TaskConduit, TaskHandle};

#[cfg(feature = "std")]
pub use conflate::{ConflateReceiver, ConflateSender};

#[cfg(feature = "std")]
pub use channel::{
    gateway, GatewayHost, GatewayRecvError, GatewaySendError, GatewaySender, GatewayStats,
};

#[cfg(feature = "std")]
pub use harness::{run_boundary_harness, FastPrng, HarnessReport, ProfileHarness};

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
/// * OLD generation (`header.generation < current`) — a stale packet from
///   a superseded generation surfacing from a queue. Rejected flat, state untouched.
/// * SAME generation — classic monotonic check: `epoch > last_seen`.
///
/// Starts at `(0, 0)`; generation `0` is the boot session, epoch `0`
/// inside ONE generation is never fresh (a new generation is always a
/// fresh re-handshake). Cost: one extra `u64`
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
    /// packets or superseded generations.
    ///
    /// A NEW generation is accepted unconditionally, epoch 0 included:
    /// the adopter must not drop the rebooted client's first packet
    /// (a rejected-but-adopted gate would cut that packet — and every
    /// retransmission of it — off forever). The "epoch 0 is never
    /// fresh" rule applies WITHIN one generation only.
    #[inline]
    pub fn observe(&mut self, header: &TransactionHeader) -> bool {
        if header.generation > self.current_generation {
            self.current_generation = header.generation;
            self.last_seen = header.epoch;
            return true;
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

    /// Non-mutating freshness check: `true` if [`EpochGate::observe`]
    /// would accept this header, WITHOUT adopting the epoch or generation.
    ///
    /// Admission pipelines peek first (cheap early rejection of stale
    /// packets) and observe only at arming: a rejection that happens
    /// after the peek (bulkhead full, rate limit) must not burn the
    /// caller's epoch — the retry with the same header stays fresh.
    #[inline]
    #[must_use]
    pub const fn peek(&self, header: &TransactionHeader) -> bool {
        if header.generation > self.current_generation {
            return true;
        }
        if header.generation < self.current_generation {
            return false;
        }
        header.epoch > self.last_seen
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

// COMPILE-TIME GATES (Layer 1 wire math, always compiled, `core`-only):
// header = 3×u64 (24 B), gate = 2×u64 (16 B); `usize` floor keeps the
// auto-sequence ticket minting off 16-bit microcontrollers.
// Pointer-width independence is PROVEN (sums of fixed primitives), not
// merely pinned: no `usize` field may ever enter a wire-crossing struct.
const _: () = {
    assert!(
        core::mem::size_of::<TransactionHeader>() == 24,
        "CRITICAL: TransactionHeader must be exactly 24 bytes (3x u64)"
    );
    assert!(
        core::mem::size_of::<TransactionHeader>() == core::mem::size_of::<u64>() * 3,
        "PORTABILITY: TransactionHeader must be 3 fixed u64, never usize"
    );
    assert!(
        core::mem::size_of::<EpochGate>() == 16,
        "CRITICAL: EpochGate must be exactly 16 bytes (2x u64)"
    );
    assert!(
        core::mem::size_of::<EpochGate>() == core::mem::size_of::<u64>() * 2,
        "PORTABILITY: EpochGate must be 2 fixed u64, never usize"
    );
    assert!(
        core::mem::size_of::<usize>() >= 4,
        "CRITICAL: snubber requires at least 32-bit usize for ticket minting"
    );
};
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
///      that hangs in a driver leaves the process in an unkillable hung state (see
///      `docs/FUTURE_TRAPS.md` Trap 2) — no crate can preempt kernel.
///
///   4. Lock-free vs the node: never call back into the same `SnubberNode`
///      (open-call rule — the node drops its lock first, but re-entrant
///      `enter` from inside a rollback is a deadlock smell).
///
///   5. Ground your own shared memory: the node's lock guards ONLY node
///      books. A rollback runs outside it (Layer-1 guards own no lock at
///      all). Every guard runs a `SeqCst` fence before the rollback body
///      (and before the `commit` disarm) — that narrows the reordering
///      window for free on x86/x64, but a fence alone pairs with NOTHING:
///      publishing to foreign state still needs `Release` and observing
///      it needs `Acquire` — or your own lock. `Relaxed` on both ends may
///      expose a stale value on another core (F13).
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
    pub fn ack<T>(&self) -> SnubberEvent<T> {
        SnubberEvent::Ack {
            header: self.header,
        }
    }

    /// Emit a `Progress` event carrying a live payload.
    pub fn progress<T>(&self, payload: T) -> SnubberEvent<T> {
        SnubberEvent::Progress {
            header: self.header,
            payload,
        }
    }

    /// Success path: disarm the destructor, report `Committed`.
    ///
    /// The rollback is taken out and `self` is forgotten so `Drop` never
    /// runs for a committed transaction. A `SeqCst` fence orders this
    /// thread's prior writes before any read after `commit` — but foreign
    /// state shared across threads still needs its own `Release`/`Acquire`
    /// pair or lock (see rule 5 below); a lone fence pairs with nothing.
    #[inline]
    pub fn commit(mut self) -> SnubberEvent<()> {
        let _ = self.rollback.take();
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let event = SnubberEvent::Committed {
            header: self.header,
        };
        mem::forget(self);
        event
    }

    /// Explicit failure path: run the rollback NOW, report `Aborted`.
    ///
    /// A `SeqCst` fence runs before the rollback body so this thread's
    /// prior writes are visible to the rollback's reads. Cross-thread
    /// publishing still needs `Release` on the writer and `Acquire` on
    /// the reader (or a lock) — the fence narrows the window, it does
    /// not invent the pairing.
    ///
    /// Use it when the fallible work returned an error value (as opposed to
    /// panicking / early-dropping, where `Drop` covers you implicitly).
    #[inline]
    pub fn abort(mut self, reason: &'static str) -> SnubberEvent<()> {
        if let Some(rollback) = self.rollback.take() {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
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
            // Memory hygiene: a `SeqCst` fence before the emergency body
            // orders this thread's prior writes ahead of the rollback's
            // reads. Same honesty as `abort` above — the fence narrows the
            // reordering window (free on x86/x64); cross-thread sharing
            // still needs a real `Release`/`Acquire` pair or a lock (F13).
            // Bullet 1 — double-panic abort: if the worker is already
            // unwinding AND the rollback itself panics, Rust aborts
            // the whole process. Under `std` we swallow the second panic;
            // without `std` there is no catcher — documented, not hidden.
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
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
        // Stale packet from the dead generation surfaces from a queue.
        let stale = TransactionHeader::with_generation(0, 1421, 1);
        assert!(!gate.observe(&stale), "old generation is rejected");
        assert_eq!(gate.last_seen(), 1, "stale packet must not move state");
        // Same-generation duplicates still cut.
        assert!(!gate.observe(&reborn), "replay inside generation cut");
        assert!(gate.observe(&TransactionHeader::with_generation(1, 2, 1)));
    }

    #[test]
    fn generation_bump_at_epoch_zero_is_a_fresh_handshake() {
        // Regression: the gate used to ADOPT the new generation and then
        // reject the epoch-0 packet. The rebooted client's first packet was
        // dropped and every retransmission of it was dropped forever — the
        // gate could never be entered from a client whose epoch starts at 0.
        let mut gate = EpochGate::new();
        assert!(
            gate.observe(&TransactionHeader::with_generation(1, 0, 1)),
            "new generation must re-handshake even at epoch 0"
        );
        assert_eq!(gate.generation(), 1);
        assert_eq!(gate.last_seen(), 0);
        // Same-generation replay at epoch 0 is still stale.
        assert!(!gate.observe(&TransactionHeader::with_generation(1, 0, 1)));
        // Later epochs flow normally.
        assert!(gate.observe(&TransactionHeader::with_generation(1, 1, 1)));
        assert!(!gate.observe(&TransactionHeader::with_generation(1, 1, 1)));
    }

    #[test]
    fn peek_is_observe_without_adoption() {
        let mut gate = EpochGate::new();
        // Fresh packet: peek agrees with observe.
        assert!(gate.peek(&TransactionHeader::new(1, 1)));
        assert!(gate.observe(&TransactionHeader::new(1, 1)));
        // Replay: both cut.
        assert!(!gate.peek(&TransactionHeader::new(1, 1)));
        assert!(!gate.observe(&TransactionHeader::new(1, 1)));
        // Higher epoch in the same generation: both fresh.
        assert!(gate.peek(&TransactionHeader::new(2, 1)));
        // New generation: peek is fresh WITHOUT adopting anything.
        assert!(gate.peek(&TransactionHeader::with_generation(3, 0, 1)));
        assert_eq!(gate.generation(), 0, "peek must not adopt");
        assert_eq!(gate.last_seen(), 1, "peek must not advance");
        // observe still adopts afterwards.
        assert!(gate.observe(&TransactionHeader::with_generation(3, 0, 1)));
        assert_eq!(gate.generation(), 3);
        // Old generation after the adoption: both cut, state untouched.
        assert!(!gate.peek(&TransactionHeader::with_generation(2, 99, 1)));
        assert!(!gate.observe(&TransactionHeader::with_generation(2, 99, 1)));
        assert_eq!(gate.last_seen(), 0);
    }

    #[test]
    fn node_gate_adopts_new_generation_at_epoch_zero() {
        let node = SnubberNode::builder("gate/gen0").build();
        assert!(
            node.observe(&TransactionHeader::with_generation(7, 0, 1)),
            "node gate must adopt the new generation, not reject gen-0 traffic"
        );
        assert!(!node.observe(&TransactionHeader::with_generation(7, 0, 1)));
        assert!(node.observe(&TransactionHeader::with_generation(7, 1, 2)));
    }

    #[test]
    #[cfg(feature = "std")]
    fn panicking_rollback_does_not_abort_process() {
        // Panicking rollback: panics inside Drop while the worker is
        // already unwinding. Without catch_unwind this is instant abort.
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
