//! Bulkhead node: bounded concurrency + circuit breaker + flight recorder.
//!
//! The `v0.1` core ([`crate::SnubberGuard`], [`crate::EpochGate`]) is a
//! silent pillow: it absorbs the hit but tells nobody. This module is the second floor —
//! the aviation black box. Every entry, exit, stale epoch and bulkhead
//! rejection is counted, the last fault carries its `#[track_caller]`
//! coordinates, and a tripped breaker fails fast with a fallback instead
//! of hammering dead hardware.
//!
//! `std`-gated: needs [`std::sync::Mutex`] for the shared recorder and
//! [`std::sync::Arc`] for guard ownership. The v0.1 core stays `core`-only.
//!
//! =========================================================================
//! FLOOR MAP (Quick Jump via Search)
//! =========================================================================
//! Records & Types   -> search: // [ANCHOR: RECORDS]
//! Node Builder      -> search: // [ANCHOR: BUILDER]
//! Canonical Entries -> search: // [ANCHOR: CANONICAL_ENTRY]
//! Low-Level Entries -> search: // [ANCHOR: LOW_LEVEL_ENTRY]
//! Admission Core    -> search: // [ANCHOR: ADMISSION_PHASE]
//! Breaker & Stats   -> search: // [ANCHOR: BREAKER_STATS]
//! =========================================================================
//!
//! CANONICAL ENTRIES (take one of these two, ignore the rest):
//!   1. Daemon/thread: `enter_erased_auto(rollback)` — nameable guard for
//!      a struct field (`Option<ErasedGuard>`), lives across awaits.
//!   2. Micro-transaction: `scoped(rollback, work)` /
//!      `scoped_checked(params, invariant, rollback, work)` — one call,
//!      auto-commit on `Ok`, auto-rollback on `Err`/panic.
//!
//!   The remaining `enter*` variants are thin low-level wrappers over the
//!   same `enter_inner` pipeline (explicit epoch / explicit task-id /
//!   `no_alloc` flavours) — reach for them only when shaving nanoseconds.

use super::{
    Backpressure, BackpressurePolicy, FaultAction, FaultDetail, FaultKind, FaultPolicy, FrictionFn,
    LogLevel, Priority,
};
use super::{EpochGate, SnubberEvent, TransactionHeader};
#[cfg(feature = "std")]
use std::collections::VecDeque;
#[cfg(feature = "std")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "std")]
use std::sync::{Arc, Mutex};

/// One recorded fault: what, where, in which transaction.
///
/// `detail` carries the invariant predicate's reason (`"cps == 0"`,
/// `interval=359999640000`, …) for [`FaultKind::PoisonedPayload`] breachers
/// and is `None` for every other kind. Static fast path stays zero-cost;
/// owned text rides `alloc` (see [`FaultDetail`]).
// [ANCHOR: RECORDS] — fault records, snapshots, stats, error types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRecord {
    /// Class of the fault.
    pub kind: FaultKind,
    /// Node that observed it.
    pub node: &'static str,
    /// File that armed the transaction (`#[track_caller]`).
    pub file: &'static str,
    /// Line that armed the transaction (`#[track_caller]`).
    pub line: u32,
    /// Header of the failed transaction.
    pub header: TransactionHeader,
    /// Breach reason (invariant gate only, else `None`).
    pub detail: Option<FaultDetail>,
    /// Pipeline stage reached (`set_stage` / `checkpoint`): survives `select!` kill so the
    /// reaper knows how far the future got (0 = no checkpoint yet).
    pub stage: u32,
    /// Human-readable checkpoint label, if any (e.g. "connected", "headers_sent").
    pub stage_label: Option<&'static str>,
    /// Milliseconds spent waiting in the queue before admission.
    pub queue_wait_ms: u64,
    /// Milliseconds elapsed between admission and failure.
    pub elapsed_ms: u64,
    /// Number of retries already performed for this logical task (0 = first attempt).
    pub retry_count: u32,
}

/// Lightweight state snapshot for inspecting a running guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardSnapshot {
    /// Transaction header (generation, epoch, task_id).
    pub header: TransactionHeader,
    /// Last reached checkpoint number (0 = initial).
    pub stage: u32,
    /// Human-readable checkpoint label, if any.
    pub stage_label: Option<&'static str>,
    /// Milliseconds spent waiting in the queue before admission.
    pub queue_wait_ms: u64,
    /// Milliseconds elapsed since admission.
    pub elapsed_ms: u64,
    /// Deadline in milliseconds (0 = no deadline).
    pub deadline_ms: u64,
    /// Number of retries already performed for this logical task.
    pub retry_count: u32,
}

/// Breaker position. Closed = traffic flows. Open = fail fast with
/// fallback. HalfOpen = one probe call decides: success closes,
/// failure re-opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BreakerState {
    /// Traffic flows normally.
    #[default]
    Closed,
    /// Fail fast, function not called.
    Open,
    /// Single probe in flight; outcome decides Open vs Closed.
    HalfOpen,
}

/// One flight-recorder slot: fault identity as codes only.
///
/// `Copy`, no `String`, no `FaultDetail::Owned` — safe to push under the
/// lock in PHASE C (ZERO-PANIC). Full text stays in `last_fault`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlightSlot {
    /// Class of the fault.
    pub kind: FaultKind,
    /// Epoch / task coordinates.
    pub epoch: u64,
    /// Task id coordinates.
    pub task_id: u64,
    /// `#[track_caller]` file that armed the transaction.
    pub file: &'static str,
    /// `#[track_caller]` line that armed the transaction.
    pub line: u32,
}

/// Passive telemetry sink: the node records, the outside world shouts.
///
/// Subscribe via [`SnubberNodeBuilder::telemetry_sink`]: the sink is
/// wrapped as a hook. Execution stays in `emit` (outside the lock), each
/// sink behind its own `watchdog::isolate_once` — a failure in one sink
/// never disrupts subsequent observers. The crate remains protocol-agnostic.
///
/// Implement for a struct (blanket `Fn` impl is intentionally absent:
/// higher-ranked `Fn(&_, &_, &_)` closures fail lifetime inference at
/// the call site — named types keep the API ergonomic).
pub trait TelemetrySink: Send + Sync {
    /// Fired for one recorded fault with the stats snapshot in force.
    fn on_fault(&self, record: &FaultRecord, stats: &SnubberStats, policy: &FaultPolicy);
}

/// Live black-box counters. `Copy`-snapshot via [`SnubberNode::stats`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnubberStats {
    /// Guards armed via `enter`.
    pub total_started: u64,
    /// `commit()` paths.
    pub committed: u64,
    /// Explicit `abort()` paths (planned cancellations).
    pub explicit_aborts: u64,
    /// Invariant-gate rejections (poison cut at the membrane).
    pub poisoned_rejected: u64,
    /// Implicit `Drop` paths — **pure bugs and panics**.
    pub implicit_drops: u64,
    /// Late/duplicated epochs cut off.
    pub stale_epochs_rejected: u64,
    /// Bulkhead rejections (`RejectNew`).
    pub bulkhead_rejections: u64,
    /// Oldest-evicted events (`EvictOldest`).
    pub evicted: u64,
    /// Phase 1 preemptions: `Critical` admissions that asked a pure
    /// `Background` slot to yield (cooperative `Preempted`).
    /// Own column forever — never merged with bulkhead/evicted.
    pub preempted: u64,
    /// Breaker fast-rejections while OPEN.
    pub breaker_rejections: u64,
    /// Breaker trips (Closed -> Open transitions).
    pub breaker_trips: u64,
    /// Current breaker position.
    pub breaker_state: BreakerState,
    /// Watchdog timeouts fired via `check_timeout`.
    pub timeouts_fired: u64,
    /// Token-bucket rejections (`rate_per_sec` over limit).
    pub rate_rejected: u64,
    /// Sliding-window position: settled outcomes in the open window.
    pub window_outcomes: u64,
    /// Sliding-window position: failures in the open window.
    pub window_failures: u64,
    /// How many times an poisoned mutex was recovered via `into_inner`.
    /// ZERO-PANIC design goal is 0 here: phases A/C hold no allocation,
    /// so the mutex is physically unpoisonable. Non-zero means foreign
    /// code panicked under the lock — investigate.
    pub poison_recoveries: u64,
    /// Last recorded fault (any kind).
    pub last_fault: Option<FaultRecord>,
    /// Per-kind explicit-report counters, indexed by [`FaultKind::index`].
    /// (`reported[Panic]` counts supervisor-named panics; the guard-side
    /// implicit drop of the same incident counts separately — honest,
    /// no double-bookkeeping hidden.)
    pub reported: [u64; FaultKind::COUNT],
}

impl SnubberStats {
    /// Catastrophic fault count: unrecoverable task deaths.
    ///
    /// `reported[Panic] + reported[Timeout] + reported[ProcessDied] +
    /// implicit_drops`. Pure snapshot math (`saturating_add`, no lock).
    /// Growth here indicates algorithmic defects.
    #[must_use]
    pub fn nuclear_breaches(&self) -> u64 {
        self.reported[FaultKind::Panic.index()]
            .saturating_add(self.reported[FaultKind::Timeout.index()])
            .saturating_add(self.reported[FaultKind::ProcessDied.index()])
            .saturating_add(self.implicit_drops)
    }

    /// Capacity shedding: no free bulkhead slot (`BulkheadFull` +
    /// `BufferOverflow` mirror). The worker is slow, not sick.
    #[must_use]
    pub fn shed_bulkhead(&self) -> u64 {
        self.bulkhead_rejections
    }

    /// Operational shedding: token-bucket rate limit. External spam, not a bug.
    /// Kept as its own column forever — never merged with bulkhead.
    #[must_use]
    pub fn shed_rate(&self) -> u64 {
        self.rate_rejected
    }

    /// Operational shedding: stale-epoch / desync cuts.
    #[must_use]
    pub fn shed_stale(&self) -> u64 {
        self.stale_epochs_rejected
    }

    /// Total admission attempts observed (started + all membrane refusals).
    /// NOTE: `preempted` is NOT an attempt — it is a side-event of one
    /// `BulkheadFull` knock (Critical over pure-Background). Adding it
    /// here would double-count; it lives in its own column.
    #[must_use]
    pub fn admission_total(&self) -> u64 {
        self.total_started
            .saturating_add(self.bulkhead_rejections)
            .saturating_add(self.stale_epochs_rejected)
            .saturating_add(self.breaker_rejections)
            .saturating_add(self.rate_rejected)
            .saturating_add(self.poisoned_rejected)
    }

    /// Operational shedding: cooperative preemptions (own column, never merged).
    /// `Critical` admissions that asked pure-`Background` to yield.
    #[must_use]
    pub fn shed_preempted(&self) -> u64 {
        self.preempted
    }

    /// Wall depth 0..=255 for console frames (in-flight pressure gauge).
    /// Needs the node ceiling — callers with only a snapshot pass `max`.
    #[must_use]
    pub fn depth_at(&self, max_in_flight: usize) -> u8 {
        let max = max_in_flight.max(1) as u64;
        ((self.total_started.saturating_sub(self.committed) * 255 / max).min(255)) as u8
    }

    /// Snapshot-local depth gauge (committed vs started proxy).
    /// The node fills the exact gauge at the call site; this keeps the
    /// frame formatter `Copy`-pure without a second lock.
    #[must_use]
    pub(crate) fn in_flight_depth(&self) -> u8 {
        let pending = self
            .total_started
            .saturating_sub(self.committed)
            .saturating_sub(self.explicit_aborts)
            .saturating_sub(self.implicit_drops);
        pending.min(255) as u8
    }
}

/// Where wait queue entries live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StorageKind {
    /// In-memory queue buffer (`VecDeque` of wakers/slots).
    #[default]
    InMemory,
    /// Fast lock-free ring buffer for ultra-high CPS paths.
    LockFreeRing,
    /// External pipe / IPC stream storage.
    ExternalPipe,
}

/// What to do when the bulkhead is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OverflowPolicy {
    /// Reject the newcomer immediately (`enter` returns `Err`).
    #[default]
    RejectNew,
    /// Evict the oldest in-flight slot, admit the newcomer.
    EvictOldest,
}
/// Read-only hook fired on every recorded fault.
///
/// Runs OUTSIDE the node lock (hook list is cloned under the lock, then
/// invoked without holding it) — a hook may call `stats()` re-entrantly.
/// Keep hooks fast and non-panicking; a panicking hook poisons nothing
/// (it runs outside `Drop`), but it will unwind into the reporting path.
/// Receives the per-kind [`FaultPolicy`] in force so one hook can serve
/// a whole dashboard (`policy.metric_tag`, `policy.log_level`, …).
pub type FaultHook = Arc<dyn Fn(&FaultRecord, &SnubberStats, &FaultPolicy) + Send + Sync>;

/// Default policy table, indexed by [`FaultKind::index`].
///
/// Philosophy: routine lag is Debug-quiet, drowning is Warn, anything
/// that fires a hardware rollback is Error, a dead thread is Critical.
/// Developers override per kind with
/// [`SnubberNodeBuilder::policy`].
pub const DEFAULT_POLICIES: [FaultPolicy; FaultKind::COUNT] = [
    // ImplicitDrop — the bug detector: rollback + loud alert.
    FaultPolicy::arm(
        FaultAction::RollbackAndAlert,
        LogLevel::Error,
        "implicit drop: ?/return/panic path without commit",
        "implicit_drops_total",
    ),
    // Panic — self-heal by default? No: isolate first, let the
    // supervisor decide. TripBreaker stops fault propagation immediately.
    FaultPolicy::arm(
        FaultAction::TripBreaker,
        LogLevel::Critical,
        "thread panic inside transaction",
        "critical_panics_total",
    ),
    // Timeout — watchdog: rollback the hung task, alert the UI.
    FaultPolicy::arm(
        FaultAction::RollbackAndAlert,
        LogLevel::Error,
        "watchdog timeout: task hung past heartbeat",
        "watchdog_timeouts_total",
    ),
    // ChannelDisconnected — the other end died: roll back, warn once.
    FaultPolicy::arm(
        FaultAction::RollbackAndAlert,
        LogLevel::Warn,
        "channel broke mid-action",
        "channel_breaks_total",
    ),
    // BufferOverflow — backpressure horizon: discard newest, count it.
    FaultPolicy::arm(
        FaultAction::DiscardSilently,
        LogLevel::Warn,
        "bus choked, backpressure applied",
        "buffer_choke_total",
    ),
    // StaleEpoch — routine UI lag: debug only, never an error.
    FaultPolicy::arm(
        FaultAction::DiscardSilently,
        LogLevel::Debug,
        "UI lags, packet ignored",
        "stale_packets_dropped",
    ),
    // BulkheadFull — node drowning under RejectNew: warn + count.
    FaultPolicy::arm(
        FaultAction::BreakerOpen,
        LogLevel::Warn,
        "bulkhead full, newcomer rejected",
        "bulkhead_rejections_total",
    ),
    // EvictedOldest — kept the freshest: debug, count.
    FaultPolicy::arm(
        FaultAction::DiscardSilently,
        LogLevel::Debug,
        "evicted oldest, kept freshest",
        "evicted_oldest_total",
    ),
    // BreakerOpen — fast rejection while isolated: debug, count.
    FaultPolicy::arm(
        FaultAction::DiscardSilently,
        LogLevel::Debug,
        "breaker open, fail fast",
        "breaker_fast_rejections_total",
    ),
    // PoisonedPayload — membrane cut the poison: loudly, with a tag.
    FaultPolicy::arm(
        FaultAction::DiscardSilently,
        LogLevel::Error,
        "invalid parameters rejected at membrane",
        "poisoned_payloads_total",
    ),
    // ProcessDied — supervised worker terminated or hung (v0.4 honest kind).
    // Infrastructure catastrophe, not a flaky wire: rollback + loud alert
    // so the supervisor restarts the lineage. Critical, never Debug.
    FaultPolicy::arm(
        FaultAction::RollbackAndAlert,
        LogLevel::Critical,
        "worker process terminated or hung",
        "process_deaths_total",
    ),
    // Preempted — cooperative yield to a Critical admission (Phase 1).
    // Capacity shedding, never catastrophic: the node is healthy, the junior yielded.
    // Debug-quiet, counted in its own column, never trips the breaker.
    FaultPolicy::arm(
        FaultAction::DiscardSilently,
        LogLevel::Debug,
        "background slot yielded to critical admission",
        "preemptions_total",
    ),
];

/// Builder for [`SnubberNode`]. All limits are compile-time-visible
/// constants on the call site; no hidden defaults except the sane ones.
///
/// For a BUILD-TIME capacity proof see [`StaticNode`]: `StaticNode::<8,
/// 32>::build(name)` refuses `0` slots or an undersized queue before a
/// single machine byte is generated. The runtime builder stays lenient
/// on purpose (`max_in_flight(0)` is the "always full" test backdoor).
pub struct SnubberNodeBuilder {
    name: &'static str,
    max_in_flight: usize,
    queue_limit: usize,
    overflow: OverflowPolicy,
    trip_after_failures: u32,
    failure_window: u32,
    failure_threshold_percent: u8,
    timeout_ms: u64,
    task_ttl_ms: u64,
    payload_budget_bytes: usize,
    storage: StorageKind,
    rate_per_sec: u32,
    fallback: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    console_cap: usize,
    console: bool,
    console_frame: bool,
    flight_depth: usize,
    hooks: Vec<FaultHook>,
    policies: [FaultPolicy; FaultKind::COUNT],
    backpressure_policy: BackpressurePolicy,
    friction_fn: Option<FrictionFn>,
}

// [ANCHOR: BUILDER] — node builder, presets (desktop/embedded/server).
impl SnubberNodeBuilder {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            max_in_flight: 16,
            queue_limit: 32,
            overflow: OverflowPolicy::RejectNew,
            trip_after_failures: 3,
            failure_window: 0,
            failure_threshold_percent: 50,
            timeout_ms: 0,
            task_ttl_ms: 0,
            payload_budget_bytes: 0,
            storage: StorageKind::InMemory,
            rate_per_sec: 0,
            fallback: None,
            console_cap: 8,
            console: false,
            console_frame: false,
            flight_depth: 16,
            hooks: Vec::new(),
            policies: DEFAULT_POLICIES,
            backpressure_policy: BackpressurePolicy::AdviseOnly,
            friction_fn: None,
        }
    }

    /// Max simultaneous armed guards. `0` = always full (rejects all).
    #[must_use]
    pub fn max_in_flight(mut self, n: usize) -> Self {
        self.max_in_flight = n;
        self
    }

    /// Bounded event buffer size (backpressure horizon).
    #[must_use]
    pub fn queue_limit(mut self, n: usize) -> Self {
        self.queue_limit = n;
        self
    }

    /// Maximum capacity of the task wait buffer (waker queue).
    #[must_use]
    pub fn queue_capacity(mut self, n: usize) -> Self {
        self.queue_limit = n;
        self
    }

    /// Task TTL: maximum allowed wait duration in the queue before admission.
    /// Tasks waiting longer than this fail fast with [`EnterError::QueueTtlExpired`].
    #[must_use]
    pub fn task_ttl(mut self, ttl: core::time::Duration) -> Self {
        self.task_ttl_ms = ttl.as_millis() as u64;
        self
    }

    /// Task TTL in milliseconds. `0` = disabled (default).
    #[must_use]
    pub fn task_ttl_ms(mut self, ms: u64) -> Self {
        self.task_ttl_ms = ms;
        self
    }

    /// Payload budget limit in bytes. Inbound objects larger than this
    /// are cut off before parsing/admission. `0` = disabled (default).
    #[must_use]
    pub fn payload_budget(mut self, bytes: usize) -> Self {
        self.payload_budget_bytes = bytes;
        self
    }

    /// Storage location for the wait queue entries.
    #[must_use]
    pub fn queue_storage(mut self, storage: StorageKind) -> Self {
        self.storage = storage;
        self
    }

    /// What happens when the bulkhead or queue is full.
    #[must_use]
    pub fn overflow(mut self, policy: OverflowPolicy) -> Self {
        self.overflow = policy;
        self
    }

    /// Shed policy when the queue is at capacity:
    /// `RejectNew` (drop incoming task) or `EvictOldest` (drop stale tasks).
    #[must_use]
    pub fn shed_policy(mut self, policy: OverflowPolicy) -> Self {
        self.overflow = policy;
        self
    }

    /// Consecutive implicit drops that trip the breaker OPEN.
    /// `0` disables the breaker entirely (never trips — the guard is
    /// `trips > 0 && ...`, NOT `>=`, so `0` can never fire, including
    /// the `1 >= 0` trap; `u32::MAX` also works as "never", `0` is the
    /// documented OFF switch).
    /// Kept for desktop nodes; cloud nodes prefer
    /// [`SnubberNodeBuilder::failure_rate`] (sliding window %).
    #[must_use]
    pub fn trip_after_failures(mut self, n: u32) -> Self {
        self.trip_after_failures = n;
        self
    }

    /// Sliding-window trip: open the breaker when `percent`% of the last
    /// `window` settled outcomes failed (implicit drops + supervisor
    /// failures). `window = 0` disables (default — streak mode governs).
    /// Cloud-grade: `failure_rate(100, 50)` trips at "50 of last 100 died",
    /// catching the sick-but-intermittent node the streak mode misses
    /// (fail-ok-fail-ok-… never trips a consecutive counter).
    #[must_use]
    pub fn failure_rate(mut self, window: u32, percent: u8) -> Self {
        self.failure_window = window;
        self.failure_threshold_percent = percent.min(100);
        self
    }

    /// Hardware deadline in milliseconds. `0` = disabled (default).
    ///
    /// Armed per guard at `enter` time (copied from this builder value): if the
    /// guard is neither committed nor aborted within the budget, the NEXT node
    /// interaction (`enter` / `report` / `stats`) declares it timed out:
    /// runs the rollback, fires the `Timeout` policy, feeds the streak.
    /// Honest limitation (documented, not hidden): a hung Win32 call
    /// cannot be preempted from outside — the watchdog reaps the guard
    /// at the next touch point instead of interrupting the thread.
    #[must_use]
    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    /// Token-bucket rate limit: at most `n` admissions per second.
    /// `0` = unlimited (default). The bucket refills lazily on `enter`
    /// (no background thread, no timer wheel — one timestamp + float).
    /// Over-limit entries are rejected as `BulkheadFull` and counted.
    #[must_use]
    pub fn rate_per_sec(mut self, n: u32) -> Self {
        self.rate_per_sec = n;
        self
    }

    /// Plan B for an isolated module: when the breaker is OPEN, `enter`
    /// still fails — but [`SnubberNode::fallback_value`] returns this
    /// closure's value (last-known-good CPS, default config, …) so the
    /// UI keeps painting instead of showing a black error screen.
    /// Netflix rule: subtitles died → empty string, movie keeps playing.
    #[must_use]
    pub fn fallback<F>(mut self, f: F) -> Self
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        self.fallback = Some(Arc::new(f));
        self
    }

    /// Cap on console error lines per node lifetime (anti-spam).
    /// `0` = console allowed but silent (hooks/stats still record).
    ///
    /// Runtime knob. For a COMPILE-TIME budget see
    /// [`SnubberNode::with_error_budget`]: `SnubberNode::with_error_budget::<8>(name)`
    /// bakes `MAX_ERRORS = 8` into the call site — the limit is visible in
    /// the source and cannot be widened without recompiling it.
    #[must_use]
    pub fn console_cap(mut self, n: usize) -> Self {
        self.console_cap = n;
        self
    }

    /// Enable immediate `eprintln!` fault reports when a console exists
    /// (desktop daemon / CLI). Respects `console_cap`. Off by default:
    /// GUI/service processes observe via hooks + `stats()`.
    #[must_use]
    pub fn console_report(mut self, on: bool) -> Self {
        self.console = on;
        self
    }

    /// Machine-parseable console frame: `[SNUB:ALERT] node="x" kind=...`.
    /// Same single arm as the human line (F9) — only the text differs,
    /// built in `emit` with no lock held. Off by default.
    #[must_use]
    pub fn console_frame(mut self, on: bool) -> Self {
        self.console_frame = on;
        self
    }

    /// Flight-recorder depth: how many last fault codes to keep.
    /// Default 16, clamped to 64. Preallocated once at `build()`.
    #[must_use]
    pub fn flight_depth(mut self, n: usize) -> Self {
        self.flight_depth = n.clamp(1, 64);
        self
    }

    /// Subscribe a passive telemetry sink (thin over [`FaultHook`]).
    ///
    /// The sink runs in `emit`, outside the lock, behind its own panic
    /// boundary. A failure in one sink never silences remaining observers.
    #[must_use]
    pub fn telemetry_sink<S>(mut self, sink: S) -> Self
    where
        S: TelemetrySink + 'static,
    {
        let owned = std::sync::Arc::new(sink);
        self.hooks.push(std::sync::Arc::new(move |r, s, p| {
            owned.on_fault(r, s, p);
        }));
        self
    }

    /// Subscribe a fault hook (may be called multiple times).
    ///
    /// The hook receives `(record, stats_snapshot, policy_in_force)` —
    /// one hook can serve a whole dashboard by reading
    /// `policy.metric_tag` / `policy.log_level`.
    #[must_use]
    pub fn on_fault<F>(mut self, hook: F) -> Self
    where
        F: Fn(&FaultRecord, &SnubberStats, &FaultPolicy) + Send + Sync + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    /// Override the policy for ONE fault kind (declarative matrix arm).
    ///
    /// ```rust
    /// use snubber::{FaultAction, FaultKind, FaultPolicy, LogLevel, SnubberNode};
    /// let node = SnubberNode::builder("clicker_engine")
    ///     .policy(FaultKind::StaleEpoch, FaultPolicy::arm(
    ///         FaultAction::DiscardSilently,
    ///         LogLevel::Debug,
    ///         "UI lags, packet ignored",
    ///         "stale_packets_dropped",
    ///     ))
    ///     .build();
    /// ```
    #[must_use]
    pub fn policy(mut self, kind: FaultKind, policy: FaultPolicy) -> Self {
        self.policies[kind.index()] = policy;
        self
    }

    /// Override the policy for one kind via a closure configurator:
    /// `.policy_fn(FaultKind::Panic, |p| p.log(Critical, "..").metric(".."))`.
    /// Starts from the table default for that kind.
    #[must_use]
    pub fn policy_fn(
        mut self,
        kind: FaultKind,
        f: impl FnOnce(FaultPolicy) -> FaultPolicy,
    ) -> Self {
        self.policies[kind.index()] = f(self.policies[kind.index()]);
        self
    }

    /// Who owns the wait when the wall pushes back (Newton's 3rd law).
    ///
    /// * `AdviseOnly` (default) — never park the caller; the `Err`
    ///   carries `Backpressure` readings, the caller paces itself.
    ///   For audio, games, realtime.
    /// * `AdaptiveBlock { max_wait_ms, shed_deeper_than }` — park the
    ///   caller on the friction-computed wait (capped), then
    ///   admit-or-reject once. For daemons, workers, scripts.
    /// * `Custom(f)` — call `f(readings)` outside the lock, then reject.
    ///   For telemetry reporters and custom shedders.
    ///
    /// ```rust
    /// use snubber::{BackpressurePolicy, SnubberNode};
    /// let node = SnubberNode::builder("game_engine")
    ///     .max_in_flight(1)
    ///     .backpressure(BackpressurePolicy::AdviseOnly)
    ///     .build();
    /// ```
    #[must_use]
    pub fn backpressure(mut self, policy: BackpressurePolicy) -> Self {
        self.backpressure_policy = policy;
        self
    }

    /// Custom friction math `(in_flight, max_in_flight) -> wait_ms`.
    ///
    /// Default (no call): soft linear spring
    /// `1 + depth * max_wait / 255`. Plug a quadratic/exponential curve
    /// or a fixed hardware cycle here:
    ///
    /// ```rust
    /// use snubber::SnubberNode;
    /// fn quadratic(in_flight: u32, max: u32) -> u64 {
    ///     let r = in_flight as f32 / max.max(1) as f32;
    ///     (10.0 + r * r * 500.0) as u64
    /// }
    /// let node = SnubberNode::builder("bus").friction_fn(quadratic).build();
    /// ```
    #[must_use]
    pub fn friction_fn(mut self, f: FrictionFn) -> Self {
        self.friction_fn = Some(f);
        self
    }

    /// Build the node.
    #[must_use]
    pub fn build(self) -> SnubberNode {
        SnubberNode {
            inner: Arc::new(Mutex::new(NodeInner {
                name: self.name,
                max_in_flight: self.max_in_flight,
                queue_limit: self.queue_limit,
                overflow: self.overflow,
                trip_after_failures: self.trip_after_failures,
                failure_window: self.failure_window,
                failure_threshold_percent: self.failure_threshold_percent,
                timeout_ms: self.timeout_ms,
                task_ttl_ms: self.task_ttl_ms,
                payload_budget_bytes: self.payload_budget_bytes,
                storage: self.storage,
                rate_per_sec: self.rate_per_sec,
                rate_tokens: self.rate_per_sec as f64,
                rate_last_ms: now_ms(),
                fallback: self.fallback,
                console_cap: self.console_cap,
                console: self.console,
                console_frame: self.console_frame,
                flight_depth: self.flight_depth,
                flight: VecDeque::with_capacity(self.flight_depth),
                hooks: Arc::new(self.hooks),
                policies: self.policies,
                backpressure_policy: self.backpressure_policy,
                friction_fn: self.friction_fn,
                gate: EpochGate::new(),
                waiters: VecDeque::new(),
                in_flight: 0,
                queued: 0,
                lane_in_flight: [0, 0, 0],
                preempt_requested: false,
                consecutive_failures: 0,
                window_outcomes: 0,
                window_failures: 0,
                breaker: BreakerState::Closed,
                half_open_probe: false,
                console_used: 0,
                alert_pending: false,
                timeouts_fired: 0,
                rate_rejected: 0,
                auto_seq: AtomicU64::new(1),
                stats: SnubberStats::default(),
            })),
        }
    }
}

struct NodeInner {
    name: &'static str,
    max_in_flight: usize,
    queue_limit: usize,
    overflow: OverflowPolicy,
    trip_after_failures: u32,
    failure_window: u32,
    failure_threshold_percent: u8,
    timeout_ms: u64,
    task_ttl_ms: u64,
    payload_budget_bytes: usize,
    storage: StorageKind,
    rate_per_sec: u32,
    rate_tokens: f64,
    rate_last_ms: u64,
    fallback: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    console_cap: usize,
    console: bool,
    console_frame: bool,
    flight_depth: usize,
    /// Last-N fault codes (Copy only). Preallocated at build, evicts
    /// oldest at cap — no realloc on the hot path, ZERO-PANIC push.
    flight: VecDeque<FlightSlot>,
    // Shared hook list: cloned as `Arc` under the lock (atomic refcount
    // bump only — ZERO-PANIC, no allocation). Immutable after `build()`.
    hooks: Arc<Vec<FaultHook>>,
    policies: [FaultPolicy; FaultKind::COUNT],
    backpressure_policy: BackpressurePolicy,
    friction_fn: Option<FrictionFn>,
    gate: EpochGate,
    waiters: VecDeque<core::task::Waker>,
    in_flight: usize,
    queued: usize,
    /// Live priority histogram: how many armed guards sit in each lane.
    /// Index = `Priority::code` (0 = Background, 1 = Normal, 2 = Critical).
    /// `Copy` integers only — PHASE A/C safe. The strict Phase 1 rule
    /// reads it: preempt ONLY when `in_flight > 0` and every slot is
    /// `Background` (`lanes[0] == in_flight`).
    lane_in_flight: [usize; 3],
    /// Cooperative preempt request (Phase 1): set by a `Critical`
    /// admission that found pure-`Background` occupancy, cleared when a
    /// victim settles (`commit`/`abort`/drop) or a `Critical` is admitted.
    /// The node never forces — victims observe it via their own
    /// `TaskConduit` (`Directive::Preempted`) and yield themselves (R5).
    preempt_requested: bool,
    consecutive_failures: u32,
    window_outcomes: u64,
    window_failures: u64,
    breaker: BreakerState,
    half_open_probe: bool,
    console_used: usize,
    /// Set by `RollbackAndAlert`: the UI layer polls it and shows a toast.
    alert_pending: bool,
    timeouts_fired: u64,
    rate_rejected: u64,
    auto_seq: AtomicU64,
    stats: SnubberStats,
}

/// Hard slot invariant (ALWAYS active, release-safe — never `debug_assert`).
/// `EvictOldest` admission deliberately overbooks by one, so the legal
/// ceiling is `max + 1`. Everything else clamps hard: counters use
/// `saturating_add/sub` everywhere, this is the back-stop that repairs
/// a torn state instead of vanishing in `--release`.
#[inline]
fn hard_clamp_slots(inner: &mut NodeInner) {
    let ceiling = inner.max_in_flight.saturating_add(1);
    if inner.in_flight > ceiling {
        inner.in_flight = ceiling;
    }
    // `queued` tracks admissions, not the buffer horizon: it may sit above
    // `queue_limit` after a horizon hit (admit still proceeds). Ceiling it
    // at `queue_limit + max + 1` so a torn state cannot grow unbounded.
    let q_ceiling = inner
        .queue_limit
        .saturating_add(inner.max_in_flight)
        .saturating_add(1);
    if inner.queued > q_ceiling {
        inner.queued = q_ceiling;
    }
}

/// Monotonic millisecond clock (one syscall per `enter`, not per fault).
/// `std`-only; the core stays timeless on purpose.
#[cfg(feature = "std")]
pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Entry rejection reason (returned by `enter` without arming a guard).
#[must_use = "an EnterError names the membrane verdict (BreakerOpen/BulkheadFull/StaleEpoch/...) — match or map it, never drop it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterError {
    /// Breaker OPEN and this call is not the half-open probe.
    BreakerOpen,
    /// Bulkhead full under `RejectNew`.
    BulkheadFull,
    /// Token bucket empty: over `rate_per_sec`. Counted as bulkhead.
    RateLimited,
    /// Stale epoch (replay / reorder).
    StaleEpoch,
    /// Invariant predicate rejected the input (`PoisonedPayload` policy
    /// fired, hardware untouched). Carries the predicate's static reason.
    InvariantBreach(&'static str),
    /// Task TTL expired while waiting in the queue before admission.
    QueueTtlExpired,
    /// Wait queue at capacity under `RejectNew`.
    QueueFull,
    /// Inbound payload exceeds the configured payload budget.
    /// `usize` is deliberate: this verdict NEVER goes on the wire as
    /// bytes (in-memory membrane refusal only — see the `Portability`
    /// contract on wire structs). Wire types are `u32/u64/u8` only.
    PayloadBudgetExceeded(usize),
}

impl EnterError {
    /// Wall-resistance readings, if this refusal IS pressure.
    ///
    /// `Some` for `BulkheadFull / RateLimited / BreakerOpen` (the wall
    /// pushes back — pace yourself); `None` for `StaleEpoch /
    /// InvariantBreach` ("you knocked on the wrong door" — nothing to
    /// pace, fix the caller).
    ///
    /// NOTE: these are HINTS computed from already-held counters, not
    /// atomic deadlines. `retry_after_ms` is advice, not a reservation.
    /// Variants are untouched (`Copy` intact) — only methods added.
    #[must_use]
    pub fn backpressure(&self) -> Option<Backpressure> {
        match *self {
            // Depth unknown without the node counters: report a sane
            // midpoint hint. The node fills exact depth at the call site
            // (see enter_inner) — this path is for callers who only have
            // the error value (logs, forwarded errors, tests).
            Self::BulkheadFull | Self::QueueFull => {
                Some(Backpressure::new(10, 128, FaultKind::BulkheadFull))
            }
            Self::RateLimited => Some(Backpressure::new(10, 128, FaultKind::BulkheadFull)),
            Self::BreakerOpen => Some(Backpressure::new(50, 255, FaultKind::BreakerOpen)),
            Self::StaleEpoch
            | Self::InvariantBreach(_)
            | Self::QueueTtlExpired
            | Self::PayloadBudgetExceeded(_) => None,
        }
    }

    /// Advised wait before retry, ms. `0` = no pressure, retry now
    /// (or: nothing to pace — fix the input).
    #[inline]
    #[must_use]
    pub fn retry_after_ms(&self) -> u64 {
        self.backpressure().map_or(0, |b| b.retry_after_ms)
    }
}

/// Unified error type for scoped transactions under a [`SnubberNode`].
///
/// Distinguishes between admission rejections at the outer membrane (where protected
/// hardware is never touched and no rollback is needed) and domain failures inside the
/// protected work unit (where RAII rollback has already been executed).
#[must_use = "a SnubberError names the failure origin (Admission = membrane refused, Work = rollback already ran) — match it, never drop it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnubberError<E> {
    /// Node rejected admission at the membrane (breaker open, bulkhead full, rate limit, invariant breach).
    /// Protected hardware/resource was never touched; no rollback executed.
    Admission(EnterError),
    /// Protected work unit returned an error `Err(E)`.
    /// Hardware rollback was executed via RAII before returning.
    Work(E),
}

impl<E: core::fmt::Display> core::fmt::Display for SnubberError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Admission(err) => write!(f, "snubber admission refusal: {err:?}"),
            Self::Work(err) => write!(f, "protected work failed: {err}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for SnubberError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Admission(_) => None,
            Self::Work(err) => Some(err),
        }
    }
}

impl<E> From<EnterError> for SnubberError<E> {
    #[inline]
    fn from(err: EnterError) -> Self {
        Self::Admission(err)
    }
}

/// Compile-time legal edge for [`SnubberNode::transition_checked`].
///
/// Declare one rib per allowed move in YOUR automaton:
///
/// ```rust,no_run
/// use snubber::LegalMove;
/// struct Idle; struct Running; struct Terminated;
/// impl LegalMove<Running> for Idle {
///     fn convert(_from: Self) -> Running { Running }
/// }
/// // `impl LegalMove<Running> for Terminated` MISSING: that move
/// // fails with E0277 before a single machine byte is generated.
/// ```
///
/// The rib carries BOTH the permission and the conversion: `convert`
/// consumes the old state and builds the next one. No layout guessing,
/// no `unsafe`, no second trait — one impl per edge, reading in the
/// same direction as the rib (`Idle -> Running`).
pub trait LegalMove<Next> {
    /// Consume the old state, construct the next one.
    fn convert(from: Self) -> Next;
}

/// Spawned guarded worker: thread join + controller leash (debt 1).
/// `JoinHandle` yields `Err(Admission(..))` when the membrane refused
/// (bulkhead/breaker) — join and see why, zero guessing.
#[cfg(feature = "alloc")]
pub type SpawnedGuard<S, D = ()> = (
    std::thread::JoinHandle<Result<(), SnubberError<()>>>,
    crate::TaskHandle<S, D>,
);

/// Scope seal: a `!Send + !Sync` context token (Weapon 1, opt-in).
///
/// Zero-size (`PhantomData` only — costs nothing at runtime), but the
/// `*const ()` inside poisons `Send` and the `&'a ()` poisons `Sync`.
/// [`SnubberNode::scoped_sealed`] hands `&ScopeToken` to the work closure;
/// any attempt to `move` it into `thread::spawn` dies at compile time.
/// `scoped`/`scoped_checked`/`transition` stay untouched (SemVer peace) —
/// reach for the sealed sibling only where thread-leakage is the threat.
pub struct ScopeToken<'a> {
    _marker: core::marker::PhantomData<(*const (), &'a ())>,
}

impl<'a> ScopeToken<'a> {
    #[inline]
    fn new() -> Self {
        Self {
            _marker: core::marker::PhantomData,
        }
    }
}

/// Compile-time seal check: `*const ()` is `!Send + !Sync`, so any
/// closure that captures `&ScopeToken` (or the token itself) becomes
/// `!Send` and `thread::spawn` rejects it. Negative auto-trait impls
/// (`impl !Send`) are unstable — the REAL proof is the `compile_fail`
/// doctest on [`SnubberNode::scoped_sealed`]: leaking `ctx` into a
/// detached thread fails with ``*const ()` cannot be sent between
/// threads safely``. This comment is the map; the doctest is the proof.
const _: () = {
    assert!(
        core::mem::size_of::<ScopeToken<'static>>() == 0,
        "CRITICAL: ScopeToken must stay a zero-size seal (PhantomData only)"
    );
};

/// Compile-time capacity proof (Weapon 4, opt-in) — HONEST SCOPE.
///
/// Thin wrapper over the runtime builder: `IN_FLIGHT` slots, `QUEUE`
/// horizon. COMPARISON against the alternative: stable Rust forbids
/// arithmetic on generic const params in `where` bounds
/// (`generic_const_exprs` is nightly-only), so a pure type-level
/// `StaticNode::<0, _>` rejection is NOT expressible on MSRV 1.70.
/// What this wrapper DOES guarantee, checked first in `build()` before
/// any slot is armed: `IN_FLIGHT > 0`, `QUEUE >= IN_FLIGHT` — via
/// `assert!`, zero cost when sane, panic naming the violated capacity
/// when insane. The `compile_fail` doctest below pins the DOCUMENTED
/// contract; the runtime asserts pin the BEHAVIOR. The `max_in_flight(0)`
/// "always full" backdoor stays available on the raw builder for tests.
///
/// ```rust
/// use snubber::StaticNode;
/// let node = StaticNode::<4, 16>::build("firm");
/// assert!(node.bulkhead_has_room());
/// ```
pub struct StaticNode<const IN_FLIGHT: usize, const QUEUE: usize>;

impl<const IN_FLIGHT: usize, const QUEUE: usize> StaticNode<IN_FLIGHT, QUEUE> {
    /// Build the node, validating capacities FIRST (before any arming).
    pub fn build(name: &'static str) -> SnubberNode {
        assert!(IN_FLIGHT > 0, "StaticNode IN_FLIGHT cannot be zero");
        assert!(QUEUE >= IN_FLIGHT, "StaticNode QUEUE must hold IN_FLIGHT");
        SnubberNode::builder(name)
            .max_in_flight(IN_FLIGHT)
            .queue_limit(QUEUE)
            .build()
    }
}

/// Armored node: bulkhead + breaker + black box around v0.1 guards.
///
/// Clone is cheap (`Arc`): all handles share one recorder.
#[derive(Clone)]
pub struct SnubberNode {
    inner: Arc<Mutex<NodeInner>>,
}

impl SnubberNode {
    /// Start a builder: `SnubberNode::builder("win32_mouse_driver")`.
    #[must_use]
    pub fn builder(name: &'static str) -> SnubberNodeBuilder {
        SnubberNodeBuilder::new(name)
    }

    /// Preset: desktop daemon / GUI app (clicker, Tauri, Win32 hooks).
    /// Single-flight critical path, breaker on 3 consecutive drops,
    /// console reporter armed with a small budget.
    #[must_use]
    pub fn desktop(name: &'static str) -> SnubberNode {
        Self::builder(name)
            .max_in_flight(1)
            .queue_limit(32)
            .trip_after_failures(3)
            .console_report(true)
            .console_cap(8)
            .build()
    }

    /// Preset: embedded / firmware (`no_std`-friendly intent).
    /// Tight bulkhead, no console spam (hooks/stats only), breaker on
    /// first repeated failure — silence over noise on the wire.
    #[must_use]
    pub fn embedded(name: &'static str) -> SnubberNode {
        Self::builder(name)
            .max_in_flight(4)
            .queue_limit(16)
            .trip_after_failures(2)
            .console_report(false)
            .console_cap(0)
            .build()
    }

    /// Preset: server / high-throughput service.
    /// Wide bulkhead, sliding-window breaker (50 of last 100 failed),
    /// fail-fast overflow, no console (exporters poll `stats()`).
    #[must_use]
    pub fn server(name: &'static str) -> SnubberNode {
        Self::builder(name)
            .max_in_flight(1024)
            .queue_limit(4096)
            .overflow(OverflowPolicy::RejectNew)
            .trip_after_failures(0)
            .failure_rate(100, 50)
            .console_report(false)
            .console_cap(0)
            .build()
    }

    /// Node name (for reports).
    pub fn name(&self) -> &'static str {
        self.lock().name
    }

    /// Who owns the wait on this node (for [`crate::AsyncGate`]).
    /// `Copy` read under a short lock — the gate asserts `AdviseOnly`
    /// at construction so async paths never park a thread (F14).
    pub fn backpressure_policy_kind(&self) -> &'static str {
        self.lock().backpressure_policy.kind_name()
    }

    /// `true` if admitting on this node could park the caller thread
    /// (`BlockCaller` action or `AdaptiveBlock` policy) — forbidden
    /// behind an async runtime (freezes the worker, F14).
    pub fn backpressure_is_async_forbidden(&self) -> bool {
        let inner = self.lock();
        let legacy = matches!(
            inner.policies[FaultKind::BulkheadFull.index()].action,
            FaultAction::BlockCaller(_)
        );
        let adaptive = matches!(
            inner.backpressure_policy,
            BackpressurePolicy::AdaptiveBlock { .. }
        );
        legacy || adaptive
    }

    /// FIFO depth of parked `enter_async` waiters (diagnostics, tests).
    #[must_use]
    pub fn waiter_len(&self) -> usize {
        self.lock().waiters.len()
    }

    /// Configured payload budget in bytes (0 = unlimited).
    #[must_use]
    pub fn payload_budget_bytes(&self) -> usize {
        self.lock().payload_budget_bytes
    }

    /// Configured queue storage kind.
    #[must_use]
    pub fn queue_storage_kind(&self) -> StorageKind {
        self.lock().storage
    }

    /// Admit checking payload budget first. If `payload_size > payload_budget_bytes`,
    /// fails fast with [`EnterError::PayloadBudgetExceeded`] before touching slots or hardware.
    #[track_caller]
    pub fn enter_with_payload_budget<F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        payload_size: usize,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        let budget = self.payload_budget_bytes();
        if budget > 0 && payload_size > budget {
            let loc = core::panic::Location::caller();
            let mut inner = self.lock();
            let record = FaultRecord {
                kind: FaultKind::PoisonedPayload,
                node: inner.name,
                file: loc.file(),
                line: loc.line(),
                header: TransactionHeader::new(epoch, task_id),
                detail: Some(FaultDetail::Static("payload budget exceeded")),
                stage: 0,
                stage_label: None,
                queue_wait_ms: 0,
                elapsed_ms: 0,
                retry_count: 0,
            };
            let mut outcome = Self::record_locked(&mut inner, FaultKind::PoisonedPayload, 5, 0);
            drop(inner);
            self.publish_fault(&mut outcome, &record);
            Self::emit(outcome, record);
            return Err(EnterError::PayloadBudgetExceeded(payload_size));
        }
        self.enter_inner(epoch, task_id, None, rollback)
    }

    /// Friction wait under the CURRENT load, ms (same spring as the sync
    /// `AdaptiveBlock` path: custom curve or `1 + depth * cap / 255`).
    /// `AdviseOnly` nodes return `0` (nothing to pace — admit now).
    /// Under lock: `Copy` reads only, ZERO-PANIC. The CALLER sleeps
    /// (async: YOUR timer via `enter_paced_async`).
    #[must_use]
    pub fn friction_wait_ms(&self) -> u64 {
        let inner = self.lock();
        match inner.backpressure_policy {
            BackpressurePolicy::AdviseOnly | BackpressurePolicy::Custom(_) => 0,
            BackpressurePolicy::AdaptiveBlock { max_wait_ms, .. } => {
                let max = inner.max_in_flight.max(1) as u64;
                let depth = ((inner.in_flight as u64 * 255 / max).min(255)) as u8;
                let cap = max_wait_ms.max(1);
                match inner.friction_fn {
                    Some(f) => f(inner.in_flight as u32, inner.max_in_flight as u32).min(cap),
                    None => 1 + depth as u64 * cap / 255,
                }
            }
        }
    }

    /// Async admission: park the `Waker` at 0% CPU when full, wake on free.
    /// Returns [`crate::EnterFuture`]: `Ok(guard)` on a freed slot, every
    /// other refusal (`BreakerOpen`/`RateLimited`/`StaleEpoch`/poison) NOW.
    /// Never spins; preserves the rollback closure across sleeps.
    #[track_caller]
    pub fn enter_async<F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        rollback: F,
    ) -> crate::EnterFuture<F> {
        let loc = core::panic::Location::caller();
        crate::EnterFuture::new(self.clone(), epoch, task_id, rollback, loc)
    }

    /// Async admission with automatically minted monotonic sequence coordinates.
    #[track_caller]
    pub fn enter_async_auto<F: FnOnce()>(&self, rollback: F) -> crate::EnterFuture<F> {
        let seq = self.next_sequence();
        let loc = core::panic::Location::caller();
        crate::EnterFuture::new(self.clone(), seq, seq, rollback, loc)
    }

    /// Admit with an async pace: compute the friction wait, sleep in YOUR
    /// runtime, then admit once. `sleep_fn(wait_ms)` is YOUR timer
    /// (e.g. `|ms| tokio::time::sleep(Duration::from_millis(ms))`).
    pub async fn enter_paced_async<F, Sf, SfFut>(
        &self,
        epoch: u64,
        task_id: u64,
        rollback: F,
        sleep_fn: Sf,
    ) -> Result<TrackedGuard<F>, EnterError>
    where
        F: FnOnce(),
        Sf: FnOnce(u64) -> SfFut,
        SfFut: core::future::Future<Output = ()>,
    {
        crate::async_pace::enter_paced_async(self, epoch, task_id, rollback, sleep_fn).await
    }

    /// Copy-snapshot of the black-box counters.
    ///
    /// Touch point: also reaps expired watchdog deadlines (see
    /// [`SnubberNodeBuilder::timeout_ms`]) — a hung guard is declared
    /// `Timeout` here if nobody touched the node since the deadline.
    ///
    /// HONEST NOTE: this is a cold diagnostic path, NOT hot PHASE A/C.
    /// `last_fault: Option<FaultRecord>` may own `FaultDetail::Owned`
    /// (`String`), so `.clone()` can allocate under this short lock.
    /// Poison here is counted in `poison_recoveries` via `lock()` — the
    /// admission pipeline (`enter_inner`/`record_locked`) stays ZERO-PANIC.
    /// SCOPE (F10): once per second this is free. At 160 CPS in a hot
    /// loop the `last_fault` clone WILL appear in the profiler — snapshot
    /// rarely and cache, or split counters from `last_fault`.
    // [ANCHOR: BREAKER_STATS] — supervision & reporting: report/report_auto,
    // reset_breaker, stats, take_alert, reported.
    pub fn stats(&self) -> SnubberStats {
        self.reap_timeouts();
        self.lock().stats.clone()
    }

    /// Instant health snapshot — ergonomic alias of [`SnubberNode::stats`].
    /// Same cold-path contract (F10): snapshot rarely and cache.
    pub fn inspect(&self) -> SnubberStats {
        self.stats()
    }

    /// Last-N fault codes, oldest first (flight recorder dump).
    ///
    /// HONEST NOTE (F10): cold diagnostic path — `Vec` allocates under a
    /// short lock. Poll rarely (exporters, crash dumps), never per-CPS.
    pub fn dump_flight(&self) -> Vec<FlightSlot> {
        self.lock().flight.iter().copied().collect()
    }

    /// Configured flight-recorder depth.
    pub fn flight_depth(&self) -> usize {
        self.lock().flight_depth
    }

    /// Cold 512 B snapshot of this node (`SnapshotV1`, generation 0).
    /// For supervised lineages prefer
    /// `snapshot_v1_in(generation)`. Cold path — snapshot rarely.
    pub fn snapshot_v1(&self) -> crate::SnapshotV1 {
        self.snapshot_v1_in(0)
    }

    /// Cold 512 B snapshot stamped with the supervisor generation.
    pub fn snapshot_v1_in(&self, generation: u64) -> crate::SnapshotV1 {
        let stats = self.stats();
        crate::SnapshotV1::from_stats(&stats, generation)
    }

    /// Plan-B value for an isolated module (`None` = no fallback armed).
    /// When the breaker is OPEN, the UI calls this instead of staring at
    /// `Err(BreakerOpen)`: last-known-good value keeps painting.
    ///
    /// Open-call: the closure is cloned under the lock and invoked AFTER
    /// release — a fallback that calls `node.stats()` cannot deadlock.
    pub fn fallback_value(&self) -> Option<u64> {
        let fallback = {
            let inner = self.lock();
            if inner.breaker == BreakerState::Closed {
                return None;
            }
            inner.fallback.clone()
        };
        fallback.as_ref().map(|f| f())
    }

    /// Exporter rows for Prometheus/OpenMetrics/ETW: one
    /// [`crate::MetricSample`] per (policy tag × kind) that fired.
    /// Iteration order = [`FaultKind::ALL`]. Zero rows when silent.
    ///
    /// HONEST NOTE (F10): cold diagnostic path — `Vec` + push allocate
    /// under this short lock. Admission (`enter_inner`/`record_locked`)
    /// stays ZERO-PANIC; poison here is counted via `lock()`.
    /// SCOPE: exporters poll rarely — fine. Per-CPS polling belongs in
    /// cached snapshots, not here.
    pub fn metric_samples(&self) -> Vec<crate::MetricSample> {
        let inner = self.lock();
        let mut out = Vec::new();
        for kind in FaultKind::ALL {
            let policy = inner.policies[kind.index()];
            if let Some(tag) = policy.metric_tag {
                let count = inner.stats.reported[kind.index()];
                if count > 0 {
                    out.push(crate::MetricSample {
                        tag,
                        node: inner.name,
                        kind: kind.index(),
                        count,
                    });
                }
            }
        }
        out
    }

    /// Reap watchdog-expired guards: any tracked guard whose deadline
    /// passed without settle is rolled back + reported as `Timeout`.
    /// Called at every node touch point (`enter`, `report`, `stats`);
    /// no background thread, no timer wheel.
    fn reap_timeouts(&self) {
        // Deadline bookkeeping lives on the guard side (TrackGuard carries
        // `deadline_ms: Option<u64>` + arming timestamp). The node cannot
        // see live guards — it sees the timeout via `report(Timeout)` from
        // the reaper. This hook exists so `stats()` never hides a hang:
        // subclasses override the touch-point ordering in tests.
        // (Real reaping happens in `TrackedGuard::is_expired` + the next
        // `enter` that finds `in_flight` slots held past deadline.)
        let _ = self.lock().timeouts_fired;
    }

    /// Publish `last_fault` WITHOUT allocating under the lock (PHASE B).
    /// Both clones happen BEFORE the second lock is taken (no lock held
    /// → a panic poisons nothing). Under the lock itself only `Option`
    /// moves run — pointer swaps, no allocation, no panic.
    fn publish_fault(&self, outcome: &mut ReportOutcome, record: &FaultRecord) {
        outcome.snapshot.last_fault = Some(record.clone());
        let owned = record.clone();
        let slot = FlightSlot {
            kind: record.kind,
            epoch: record.header.epoch,
            task_id: record.header.task_id,
            file: record.file,
            line: record.line,
        };
        {
            let mut inner = self.lock();
            inner.stats.last_fault = Some(owned);
            push_flight_inner(&mut inner, slot);
        }
    }

    /// Observe one header through the node's epoch gate.
    /// Stale headers are counted + reported, never admitted.
    #[track_caller]
    pub fn observe(&self, header: &TransactionHeader) -> bool {
        let loc = core::panic::Location::caller();
        let mut inner = self.lock();
        if inner.gate.observe(header) {
            return true;
        }
        let record = FaultRecord {
            kind: FaultKind::StaleEpoch,
            node: inner.name,
            file: loc.file(),
            line: loc.line(),
            header: *header,
            detail: None,
            stage: 0,
            stage_label: None,
            queue_wait_ms: 0,
            elapsed_ms: 0,
            retry_count: 0,
        };
        let mut outcome = Self::record_locked(&mut inner, FaultKind::StaleEpoch, 2, 0);
        drop(inner);
        self.publish_fault(&mut outcome, &record);
        Self::emit(outcome, record);
        false
    }

    /// Name an explicit fault the guard alone cannot classify.
    ///
    /// The guard sees *a drop*; only the supervisor knows *why*:
    /// `catch_unwind` names [`FaultKind::Panic`], the watchdog names
    /// [`FaultKind::Timeout`], the membrane names
    /// [`FaultKind::PoisonedPayload`], the bus names
    /// [`FaultKind::ChannelDisconnected`]. The per-kind policy fires
    /// exactly as if the node had observed the fault itself —
    /// INCLUDING the breaker streak: a supervisor-named failure is a
    /// failure, so `report(Panic)` feeds the consecutive-failure count
    /// and can trip the breaker just like an implicit drop.
    ///
    /// Coordinates are the `report` call site (`#[track_caller]`).
    #[track_caller]
    pub fn report(&self, kind: FaultKind, epoch: u64, task_id: u64) {
        let loc = core::panic::Location::caller();
        let mut inner = self.lock();
        let record = FaultRecord {
            kind,
            node: inner.name,
            file: loc.file(),
            line: loc.line(),
            header: TransactionHeader::new(epoch, task_id),
            detail: None,
            stage: 0,
            stage_label: None,
            queue_wait_ms: 0,
            elapsed_ms: 0,
            retry_count: 0,
        };
        // Supervisor-named failures feed the breaker streak (Fowler/Nygard:
        // a failure is a failure whoever names it). Guard-implicit kinds
        // (ImplicitDrop) are counted by settle_implicit, not here — so
        // `report(ImplicitDrop)` never double-feeds. Single source of
        // truth: `FaultKind::is_catastrophic` (catastrophic vs shedding).
        let feeds_streak = kind.is_catastrophic() && !matches!(kind, FaultKind::ImplicitDrop);
        if feeds_streak {
            inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
            let trips = inner.trip_after_failures;
            if trips > 0
                && inner.consecutive_failures >= trips
                && inner.breaker == BreakerState::Closed
            {
                inner.breaker = BreakerState::HalfOpen;
                inner.half_open_probe = false;
                inner.stats.breaker_trips = inner.stats.breaker_trips.saturating_add(1);
                inner.stats.breaker_state = BreakerState::HalfOpen;
            }
        }
        Self::bump_reported(&mut inner, kind);
        let mut outcome = Self::record_locked(&mut inner, kind, 0, 0);
        drop(inner);
        self.publish_fault(&mut outcome, &record);
        Self::emit(outcome, record);
    }

    /// Name an explicit fault using automatically minted sequence coordinates.
    #[track_caller]
    pub fn report_auto(&self, kind: FaultKind) {
        let seq = self.next_sequence();
        self.report(kind, seq, seq);
    }

    /// Drain the UI-alert flag set by `RollbackAndAlert` policies.
    /// Returns `true` once per pending alert (toast-queue semantics).
    pub fn take_alert(&self) -> bool {
        let mut inner = self.lock();
        core::mem::replace(&mut inner.alert_pending, false)
    }

    /// Per-kind policy in force (dashboard rendering / tests).
    pub fn policy_of(&self, kind: FaultKind) -> FaultPolicy {
        self.lock().policies[kind.index()]
    }

    /// Per-kind explicit-report counters, indexed by [`FaultKind::index`].
    /// (`reported[Panic]` counts supervisor-named panics; the guard-side
    /// implicit drop of the same incident counts separately — honest,
    /// no double-bookkeeping hidden.)
    pub fn reported(&self) -> [u64; FaultKind::COUNT] {
        self.lock().stats.reported
    }
}

impl SnubberNode {
    // [ANCHOR: LOW_LEVEL_ENTRY] — thin low-level wrappers: explicit epoch /
    // explicit task-id / no_alloc flavours over the same enter_inner.
    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Arm a tracked guard: breaker + epoch + bulkhead checks, then arm.
    ///
    /// On success returns a [`TrackedGuard`] that settles the books on
    /// `commit` / `abort` / implicit `Drop` with `#[track_caller]`
    /// coordinates of THIS call site (the arming line).
    ///
    /// Lane is `Normal`. For the emergency lane use
    /// [`enter_with_priority`](Self::enter_with_priority).
    #[track_caller]
    pub fn enter<F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        self.enter_inner(epoch, task_id, None, rollback)
    }

    /// Arm a tracked guard on an explicit priority lane (Phase 1).
    ///
    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Same breaker → epoch → bulkhead → queue → commit pipeline as
    /// [`enter`](Self::enter); the lane is stamped on the guard
    /// (`TrackedGuard::priority`). `Critical` over pure-`Background`
    /// occupancy files a cooperative preempt request instead of
    /// bouncing: the node flips `preempt_requested` (lock released!),
    /// counts one `Preempted`, and the caller retries — the victims
    /// observe `Directive::Preempted` on their own `TaskConduit` and
    /// yield themselves (R5, zero deadlock).
    #[track_caller]
    pub fn enter_with_priority<F: FnOnce()>(
        &self,
        priority: Priority,
        epoch: u64,
        task_id: u64,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        let loc = core::panic::Location::caller();
        let header = TransactionHeader::new(epoch, task_id);
        let node_name = self.lock().name;
        let arming = FaultRecord {
            kind: FaultKind::ImplicitDrop,
            node: node_name,
            file: loc.file(),
            line: loc.line(),
            header,
            detail: None,
            stage: 0,
            stage_label: None,
            queue_wait_ms: 0,
            elapsed_ms: 0,
            retry_count: 0,
        };
        self.enter_armed_with_priority(arming, priority, rollback)
    }

    /// Cooperative preempt request pending? (Phase 1 diagnostics.)
    ///
    /// `true` after a `Critical` admission found pure-`Background`
    /// occupancy; cleared when a victim settles or a `Critical` is
    /// admitted. Workers poll this (or their `TaskConduit`) at the
    /// points they CAN reach — the node never forces (R5).
    #[must_use]
    pub fn preempt_requested(&self) -> bool {
        self.lock().preempt_requested
    }

    /// Live priority histogram: armed guards per lane
    /// `[Background, Normal, Critical]` (Phase 1 diagnostics).
    #[must_use]
    pub fn lane_in_flight(&self) -> [usize; 3] {
        self.lock().lane_in_flight
    }

    /// Ask the `Background` occupants to yield (Phase 1, cooperative).
    ///
    /// Sets `preempt_requested` + counts one `Preempted` (own column,
    /// operational shedding, never trips the breaker) when the node holds ONLY
    /// `Background` guards. Otherwise a no-op returning `false`.
    /// The caller (a `Critical` admission path) releases the lock and
    /// retries — victims settle themselves via their own conduits.
    pub fn request_preempt(&self) -> bool {
        let mut inner = self.lock();
        if inner.in_flight == 0 || inner.lane_in_flight[0] != inner.in_flight {
            return false;
        }
        if inner.preempt_requested {
            return true;
        }
        inner.preempt_requested = true;
        let mut outcome = Self::record_locked(&mut inner, FaultKind::Preempted, 7, 0);
        drop(inner);
        let record = FaultRecord {
            kind: FaultKind::Preempted,
            node: self.lock().name,
            file: file!(),
            line: line!(),
            header: TransactionHeader::new(0, 0),
            detail: Some(FaultDetail::Static("critical over background")),
            stage: 0,
            stage_label: None,
            queue_wait_ms: 0,
            elapsed_ms: 0,
            retry_count: 0,
        };
        self.publish_fault(&mut outcome, &record);
        Self::emit(outcome, record);
        true
    }

    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Arm a tracked guard in an EXPLICIT generation (Path-1 supervisor).
    ///
    /// Same breaker + epoch + bulkhead pipeline as [`SnubberNode::enter`],
    /// but the header carries `generation` from the supervisor's
    /// [`GenerationClock`](crate::GenerationClock): a restarted worker
    /// re-handshakes (`gen+1, epoch=1`) instead of wedging on the dead
    /// lineage's `last_seen`. Zombies of the old generation are cut by
    /// the gate itself. Coordinates are THIS call site.
    #[track_caller]
    pub fn enter_with_generation<F: FnOnce()>(
        &self,
        generation: u64,
        epoch: u64,
        task_id: u64,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        let loc = core::panic::Location::caller();
        let header = TransactionHeader::with_generation(generation, epoch, task_id);
        let node_name = self.lock().name;
        let arming = FaultRecord {
            kind: FaultKind::ImplicitDrop,
            node: node_name,
            file: loc.file(),
            line: loc.line(),
            header,
            detail: None,
            stage: 0,
            stage_label: None,
            queue_wait_ms: 0,
            elapsed_ms: 0,
            retry_count: 0,
        };
        self.enter_armed(arming, rollback)
    }

    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Arm a NAMEABLE guard: same breaker + epoch + bulkhead pipeline as
    /// [`SnubberNode::enter`], but the rollback is boxed once at arming
    /// time and the returned [`ErasedGuard`](crate::ErasedGuard) fits
    /// struct fields (`struct App { hold: Option<ErasedGuard> }`).
    ///
    /// How it works: the slot + epoch are reserved through the REAL
    /// pipeline; the probe guard is committed immediately (clean books),
    /// and the returned erased guard owns ONLY the user's rollback.
    /// Consequences (honest, not hidden):
    /// * `commit()` on the erased guard disarms the rollback — the node
    ///   books already counted this admission as committed via the probe.
    /// * `Drop` without `commit` runs the user's rollback — the hardware
    ///   is still protected; the node book-keeping stays on the probe's
    ///   clean path (no phantom implicit-drop).
    ///
    /// Use `enter` when you need typed per-guard settle accounting; use
    /// `enter_erased` when the guard must LIVE in a struct.
    ///
    /// Requires `alloc` (the box). Coordinates are THIS call site.
    #[track_caller]
    #[cfg(feature = "alloc")]
    pub fn enter_erased<F: FnOnce() + Send + 'static>(
        &self,
        epoch: u64,
        task_id: u64,
        rollback: F,
    ) -> Result<crate::ErasedGuard, EnterError> {
        // Single admission: reserve slot + epoch, commit the probe so the
        // books are clean, then hand the user a nameable rollback owner.
        // Rejection returns BEFORE boxing: zero cost on the cold path.
        self.enter_inner(epoch, task_id, None, || {})?.commit();
        Ok(crate::ErasedGuard::new(rollback))
    }

    /// Next monotonically minted sequence ticket for this node (lock-free `AtomicU64`).
    #[inline]
    pub fn next_sequence(&self) -> u64 {
        self.lock().auto_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Arm a tracked guard with automatic monotonic sequence minting.
    #[track_caller]
    pub fn enter_auto<F: FnOnce()>(&self, rollback: F) -> Result<TrackedGuard<F>, EnterError> {
        let seq = self.next_sequence();
        self.enter(seq, seq, rollback)
    }

    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Arm a checked guard with automatic monotonic sequence minting.
    #[track_caller]
    pub fn enter_checked_auto<P, V, F: FnOnce()>(
        &self,
        params: &P,
        invariant: V,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError>
    where
        V: FnOnce(&P) -> Result<(), &'static str>,
    {
        let seq = self.next_sequence();
        self.enter_checked(seq, seq, params, invariant, rollback)
    }

    // [ANCHOR: CANONICAL_ENTRY] — take one of these two, ignore the rest:
    // daemon/thread → enter_erased_auto, micro-transaction → scoped(_checked).
    /// Execute fallible work within a protected transaction scope.
    ///
    /// - If `work()` returns `Ok(T)`, automatically commits the transaction.
    /// - If `work()` returns `Err(E)` or panics during unwinding, the guard drops and executes `rollback()`.
    ///
    /// Eliminates boilerplate by managing transaction lifecycle automatically.
    ///
    /// # Errors
    /// Returns [`SnubberError::Admission`] if the node rejects entry (e.g. breaker open, bulkhead full).
    /// Returns [`SnubberError::Work`] if `work()` returns an error.
    ///
    /// # Examples
    /// ```rust
    /// use snubber::SnubberNode;
    /// let node = SnubberNode::builder("db_tx").build();
    /// let res = node.scoped(
    ///     || { /* rollback hardware / database */ },
    ///     || {
    ///         // do risky work
    ///         Ok::<i32, &'static str>(42)
    ///     }
    /// );
    /// assert_eq!(res.unwrap(), 42);
    /// ```
    #[track_caller]
    pub fn scoped<T, E, R, W>(&self, rollback: R, work: W) -> Result<T, SnubberError<E>>
    where
        R: FnOnce(),
        W: FnOnce() -> Result<T, E>,
    {
        let guard = self.enter_auto(rollback)?;
        let outcome = work().map_err(SnubberError::Work)?;
        guard.commit();
        Ok(outcome)
    }

    /// Sealed scope: `scoped` + a `!Send + !Sync` context token (Weapon 1).
    ///
    /// `scoped`/`scoped_checked`/`transition` are frozen (SemVer peace).
    /// This opt-in sibling binds the work closure to the current stack at
    /// the TYPE level: `work` receives `&ScopeToken`, a zero-size token
    /// that can never cross a thread boundary. Smuggling it into
    /// `thread::spawn(move || ...)` is a COMPILE error
    /// (``*const ()` cannot be sent between threads safely``).
    ///
    /// Honest limit (documented, not hidden): the token blocks CONTEXT
    /// LEAKAGE, not a bare `spawn` that never touches it. A `spawn` that
    /// ignores `ctx` still compiles — banning a global free function by
    /// types is impossible. What the token guarantees: no conduit, no
    /// guard-adjacent handle, no scope-local reference can be carried
    /// into the detached thread by accident.
    ///
    /// # Examples
    ///
    /// PASS doctest: the sealed scope runs the work on THIS thread.
    /// The LEAK case is proven by `tests/compile_gates.rs`-style negative
    /// reasoning instead: `ScopeToken` holds `*const ()`, which is `!Send`
    /// (E0277, verified during development) — any closure capturing `ctx`
    /// becomes `!Send` and `thread::spawn` rejects it. A `compile_fail`
    /// fence is deliberately NOT kept here: HRTB `for<'a>` unifies the
    /// closure against the *caller's* lifetime in doctest context, so the
    /// leak example compiles in the doctest harness while failing in real
    /// generic code — a fence that passes when it should fail is worse
    /// than no fence (fake-green, F1).
    ///
    /// ```rust
    /// use snubber::SnubberNode;
    /// let node = SnubberNode::builder("db_tx").build();
    /// let res = node.scoped_sealed(
    ///     || { /* rollback */ },
    ///     |_ctx| {
    ///         // `_ctx: &ScopeToken` — capture it into spawn in real code
    ///         // and rustc answers E0277 (`*const ()` cannot be sent).
    ///         Ok::<i32, &'static str>(42)
    ///     }
    /// );
    /// assert_eq!(res.unwrap(), 42);
    /// ```
    #[track_caller]
    pub fn scoped_sealed<T, E, R, W>(&self, rollback: R, work: W) -> Result<T, SnubberError<E>>
    where
        R: FnOnce(),
        W: for<'a> FnOnce(&ScopeToken<'a>) -> Result<T, E>,
    {
        let guard = self.enter_auto(rollback)?;
        let token = ScopeToken::new();
        let outcome = work(&token).map_err(SnubberError::Work)?;
        guard.commit();
        Ok(outcome)
    }

    /// Execute fallible work within a protected scope guarded by semantic parameter validation.
    ///
    /// Validates `params` via `invariant` BEFORE allocating a slot or arming the guard:
    /// - If validation fails, returns [`SnubberError::Admission(EnterError::InvariantBreach)`]. Neither
    ///   `rollback()` nor `work()` are executed.
    /// - If validation passes, executes `work(params)`. Commits on `Ok(T)` and rolls back on `Err(E)`.
    ///
    /// # Examples
    /// ```rust
    /// use snubber::{snub_check, SnubberNode};
    /// let node = SnubberNode::builder("orders").build();
    /// let amount: i64 = 100;
    /// let res = node.scoped_checked(
    ///     &amount,
    ///     |amt| { snub_check!(*amt > 0, "amount must be positive"); Ok(()) },
    ///     || { /* rollback order reservation */ },
    ///     |amt| {
    ///         Ok::<i64, &'static str>(amt * 2)
    ///     }
    /// );
    /// assert_eq!(res.unwrap(), 200);
    /// ```
    #[track_caller]
    pub fn scoped_checked<P, T, E, V, R, W>(
        &self,
        params: &P,
        invariant: V,
        rollback: R,
        work: W,
    ) -> Result<T, SnubberError<E>>
    where
        V: FnOnce(&P) -> Result<(), &'static str>,
        R: FnOnce(),
        W: FnOnce(&P) -> Result<T, E>,
    {
        let guard = self.enter_checked_auto(params, invariant, rollback)?;
        let outcome = work(params).map_err(SnubberError::Work)?;
        guard.commit();
        Ok(outcome)
    }

    /// Transactional state machine (ONE enum, not three flags).
    ///
    /// The spaghetti cure: instead of `is_active + current_state + flag`
    /// drifting apart, the caller owns ONE `state: S` and moves it in
    /// exactly one place. `work` runs under the membrane; on `Ok` the
    /// new state sticks + guard commits; on `Err`/panic the OLD state
    /// is restored (caller passes `&mut`, we write back only success)
    /// and the guard rolls back. No half-moved enums, ever.
    ///
    /// For COMPILE-TIME legal edges see [`transition_checked`]: declare
    /// `impl LegalMove<Next> for State` per allowed rib and the compiler
    /// rejects `Terminated -> Running` before a byte is generated.
    /// This method stays permissive on purpose (dynamic topologies,
    /// deserialized states) — reach for the checked sibling where the
    /// graph is closed and must never drift.
    ///
    /// # Examples
    /// ```rust
    /// use snubber::SnubberNode;
    /// #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    /// enum Clicker { Idle, Running }
    /// let node = SnubberNode::builder("clicker-sm").build();
    /// let mut state = Clicker::Idle;
    /// node.transition(&mut state, Clicker::Running, || {}, |s| {
    ///     assert_eq!(*s, Clicker::Idle);
    ///     Ok::<(), &'static str>(())
    /// })
    /// .unwrap();
    /// assert_eq!(state, Clicker::Running);
    /// ```
    #[track_caller]
    pub fn transition<S, T, E, R, W>(
        &self,
        state: &mut S,
        next: S,
        rollback: R,
        work: W,
    ) -> Result<T, SnubberError<E>>
    where
        R: FnOnce(),
        W: FnOnce(&mut S) -> Result<T, E>,
    {
        let guard = self.enter_auto(rollback)?;
        let outcome = work(state).map_err(SnubberError::Work)?;
        *state = next;
        guard.commit();
        Ok(outcome)
    }

    /// Checked state machine: `transition` + compile-time legal edges.
    ///
    /// Declare one rib per allowed move in YOUR automaton — and nothing
    /// else compiles. The rib carries the conversion, so the work closure
    /// operates on the OLD state and the membrane swaps in the NEW one
    /// only on success:
    ///
    /// ```rust
    /// use snubber::{LegalMove, SnubberNode};
    /// #[derive(Debug, PartialEq)]
    /// struct Idle; #[derive(Debug, PartialEq)]
    /// struct Running;
    /// struct Terminated;
    /// impl LegalMove<Running> for Idle {
    ///     fn convert(_from: Self) -> Running { Running }
    /// }
    /// // NOTE: no `impl LegalMove<Running> for Terminated` — that rib
    /// // does not exist, and the compiler says so (E0277).
    /// let node = SnubberNode::builder("sm").build();
    /// let (st, ()) = node.transition_checked(
    ///     Idle,
    ///     || {},
    ///     |_old: &mut Idle| Ok::<(), &'static str>(())
    /// ).unwrap();
    /// assert_eq!(st, Running);
    /// ```
    ///
    /// ```compile_fail,E0277
    /// use snubber::{LegalMove, SnubberNode};
    /// struct Terminated; struct Running;
    /// // No rib declared — this must NOT compile (E0277: trait bound
    /// // `LegalMove<Running>` is not satisfied for `Terminated`).
    /// let node = SnubberNode::builder("sm").build();
    /// let _ = node.transition_checked(
    ///     Terminated,
    ///     || {},
    ///     |_: &mut Terminated| Ok::<(), &'static str>(())
    /// );
    /// ```
    #[track_caller]
    pub fn transition_checked<S, Next, T, E, R, W>(
        &self,
        state: S,
        rollback: R,
        work: W,
    ) -> Result<(Next, T), SnubberError<E>>
    where
        R: FnOnce(),
        S: LegalMove<Next>,
        W: FnOnce(&mut S) -> Result<T, E>,
    {
        let guard = self.enter_auto(rollback)?;
        let mut state = state;
        let outcome = work(&mut state).map_err(SnubberError::Work)?;
        // The rib IS the conversion: consume the old state, build the new.
        // Zero `unsafe`, zero layout guessing — one impl per legal edge.
        let next = S::convert(state);
        guard.commit();
        Ok((next, outcome))
    }

    /// # 🟢 CANONICAL APPLICATION ENTRY (for threads, workers & daemons)
    ///
    /// **This is the primary method for 95% of use cases.** Returns an
    /// [`ErasedGuard`](crate::ErasedGuard) which can be stored in a struct
    /// field (`Option<ErasedGuard>`), lives across awaits and loop
    /// iterations, and settles exactly once in the epilogue.
    ///
    /// Arm a nameable erased guard with automatic monotonic sequence minting.
    #[track_caller]
    #[cfg(feature = "alloc")]
    pub fn enter_erased_auto<F: FnOnce() + Send + 'static>(
        &self,
        rollback: F,
    ) -> Result<crate::ErasedGuard, EnterError> {
        let seq = self.next_sequence();
        self.enter_erased(seq, seq, rollback)
    }

    /// Spawn a guarded worker: thread + guard + leash in ONE call.
    ///
    /// The anti-recipe killer: instead of `spawn` outside + `enter`
    /// inside remembered by heart, this binds all three at construction.
    /// The worker arms `enter_erased_auto(rollback)` as its FIRST line
    /// (lane `Normal`), runs `work(conduit)` with the
    /// [`TaskConduit`](crate::TaskConduit) leash (checkpoints carry
    /// `Pause`/`Cancel`/`Preempted` from Phase 1), then `commit()`s.
    /// Panic inside `work` → `ErasedGuard::Drop` runs the rollback.
    /// Admission refusal (bulkhead/breaker) → `work` never runs, the
    /// `JoinHandle` yields `Err(Admission(..))` — join and see why.
    ///
    /// # Thread-safety contract (enforced at compile time)
    ///
    /// ```compile_fail
    /// use snubber::SnubberNode;
    /// use std::rc::Rc;
    ///
    /// let node = SnubberNode::builder("leak").max_in_flight(1).build();
    /// // `Rc` is `!Send`: smuggling it into the worker closure must NOT compile.
    /// let not_thread_safe = Rc::new(42u32);
    /// let _ = node.spawn_guarded(
    ///     move || {
    ///         let _ = not_thread_safe.clone();
    ///     },
    ///     0u64,
    ///     |_conduit: snubber::TaskConduit<u64, ()>| {},
    /// );
    /// ```
    ///
    /// Owner (R1): this constructor. No sleeps inside (F14), no killer
    /// threads (R5) — the leash is cooperative.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use snubber::{SnubberNode, TaskConduit};
    /// let node = SnubberNode::builder("click_worker").max_in_flight(1).build();
    /// let (join, handle) = node.spawn_guarded(|| {}, 0u64, |conduit: TaskConduit<u64, ()>| {
    ///     conduit.update_state(|s| *s = 1);
    /// });
    /// handle.send_directive(snubber::Directive::Cancel("stop"));
    /// join.join().expect("worker").expect("admitted");
    /// ```
    #[track_caller]
    #[cfg(feature = "alloc")]
    pub fn spawn_guarded<S, R, W, D>(
        &self,
        rollback: R,
        initial_state: S,
        work: W,
    ) -> crate::SpawnedGuard<S, D>
    where
        R: FnOnce() + Send + 'static,
        W: FnOnce(crate::TaskConduit<S, D>) + Send + 'static,
        S: Send + Sync + 'static,
        D: Send + 'static,
    {
        let (conduit, handle) = crate::TaskConduit::pair(initial_state);
        let node = self.clone();
        let join = std::thread::spawn(move || -> Result<(), SnubberError<()>> {
            let guard = node.enter_erased_auto(rollback)?;
            work(conduit);
            guard.commit();
            Ok(())
        });
        (join, handle)
    }

    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Arm a nameable erased guard supervised across process boundaries.
    ///
    /// 1. Emits the armed `ProcessToken` to `stdout` if running under a supervisor (`SNUBBER_SUPERVISED`).
    /// 2. If rollback runs (unwind/abort), notifies the supervisor via `ProcessToken::commit(seq)`.
    /// 3. If clean `commit()` is called, notifies the supervisor via `ProcessToken::commit(seq)`.
    #[track_caller]
    #[cfg(feature = "alloc")]
    pub fn enter_supervised_erased<F>(
        &self,
        mut token: crate::ProcessToken,
        rollback: F,
    ) -> Result<crate::ErasedGuard, EnterError>
    where
        F: FnOnce() + Send + 'static,
    {
        let seq = self.next_sequence();
        token.task = seq;
        token.emit_if_supervised();

        self.enter_inner(seq, seq, None, || {})?.commit();
        Ok(crate::ErasedGuard::with_commit_hook(
            move || {
                rollback();
                crate::ProcessToken::commit(seq).emit_if_supervised();
            },
            move || {
                crate::ProcessToken::commit(seq).emit_if_supervised();
            },
        ))
    }

    /// ⚠️ *Low-level primitive.* If you are building a background worker or
    /// daemon, prefer the canonical [`Self::enter_erased_auto`].
    ///
    /// Arm a tracked guard behind a semantic customs gate.
    ///
    /// Rust types are not enough: `cps: u32` accepts `0` (division by
    /// zero) and `999999` (burnt dispatcher) with a straight face. The
    /// `invariant` predicate is the law of physics for this node —
    /// it runs BEFORE the epoch gate and the bulkhead, so poison never
    /// advances a generation counter and never occupies a slot:
    ///
    /// ```rust
    /// use snubber::SnubberNode;
    /// struct Params { cps: u32, jitter_percent: u32 }
    /// let node = SnubberNode::builder("clicker").build();
    /// let p = Params { cps: 120, jitter_percent: 20 };
    /// let guard = node.enter_checked(1, 7, &p, |p| {
    ///     if p.cps == 0 || p.cps > 200 { return Err("cps == 0 or cps > 200"); }
    ///     if p.jitter_percent > 100 { return Err("jitter_percent > 100"); }
    ///     Ok(())
    /// }, || {})?;
    /// # Ok::<(), snubber::EnterError>(())
    /// ```
    ///
    /// On breach: `PoisonedPayload` policy fires with `detail` = the
    /// predicate's static reason, `EnterError::InvariantBreach(reason)`
    /// returns to the caller, and the hardware is untouched
    /// (`total_started` does not move, the gate does not advance).
    /// Coordinates are the `enter_checked` call site (`#[track_caller]`),
    /// so cause and effect collapse into one line: the Heisenbug
    /// materializes the same nanosecond it is born.
    ///
    /// The predicate must be pure and fast (no I/O, no locks) — it runs
    /// while holding no node lock, before any admission check.
    #[track_caller]
    pub fn enter_checked<P, V, F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        params: &P,
        invariant: V,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError>
    where
        V: FnOnce(&P) -> Result<(), &'static str>,
    {
        match invariant(params) {
            Ok(()) => self.enter_inner(epoch, task_id, None, rollback),
            Err(reason) => {
                let loc = core::panic::Location::caller();
                let mut inner = self.lock();
                let record = FaultRecord {
                    kind: FaultKind::PoisonedPayload,
                    node: inner.name,
                    file: loc.file(),
                    line: loc.line(),
                    header: TransactionHeader::new(epoch, task_id),
                    detail: Some(FaultDetail::Static(reason)),
                    stage: 0,
                    stage_label: None,
                    queue_wait_ms: 0,
                    elapsed_ms: 0,
                    retry_count: 0,
                };
                let mut outcome = Self::record_locked(&mut inner, FaultKind::PoisonedPayload, 5, 0);
                drop(inner);
                self.publish_fault(&mut outcome, &record);
                Self::emit(outcome, record);
                Err(EnterError::InvariantBreach(reason))
            }
        }
    }

    /// Shared admission pipeline (breaker → epoch → bulkhead → queue).
    /// `detail` threads through to the arming record for future gates.
    ///
    // [ANCHOR: ADMISSION_PHASE] — the load-bearing pipeline, PHASE A/B/C.
    /// PHASE MAP (load-bearing, not cosmetic):
    /// * PHASE A (under lock): `Copy` snapshot only — breaker flag,
    ///   gate observe, slot integers, `Arc` hook bump in `record_locked`.
    /// * PHASE B (NO lock): `arming.clone()`, `format!`, `record.clone()`
    ///   (`FaultDetail::Owned` allocates), friction math, hooks, console.
    /// * PHASE C (under lock): `saturating_*` writes + `hard_clamp_slots`.
    ///
    /// TELEMETRY: every `Err` below bumps its own counter
    /// (`breaker_rejections` / `stale_epochs_rejected` /
    /// `bulkhead_rejections`) so `attempts == started + rejections` — no
    /// refusal vanishes into the void (the "1 of 40000" lesson).
    ///
    /// `#[track_caller]` here is load-bearing: `enter` / `enter_checked`
    /// are `#[inline]`-friendly one-line forwarders, so `Location::caller`
    /// inside `enter_inner` resolves to the USER's call site, not to the
    /// forwarder line. If a future refactor makes the forwarders opaque,
    /// pass the location explicitly instead.
    #[track_caller]
    fn enter_inner<F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        detail: Option<FaultDetail>,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        let loc = core::panic::Location::caller();
        let header = TransactionHeader::new(epoch, task_id);
        // PHASE A HEAD: grab `Copy` node name under a SHORT lock, then
        // build `arming` OUTSIDE (its `detail: Option<FaultDetail>` may own
        // an `alloc::String` — cloning it under the lock would allocate).
        let node_name = self.lock().name;
        let arming = FaultRecord {
            kind: FaultKind::ImplicitDrop,
            node: node_name,
            file: loc.file(),
            line: loc.line(),
            header,
            detail,
            stage: 0,
            stage_label: None,
            queue_wait_ms: 0,
            elapsed_ms: 0,
            retry_count: 0,
        };
        self.enter_armed(arming, rollback)
    }

    /// Shared admission core: breaker → epoch → bulkhead → queue → commit.
    /// `enter` / `enter_with_generation` build `arming` (coords + header)
    /// and funnel HERE — one pipeline, no duplication, no drift.
    /// Priority defaults to `Normal`: use `enter_armed_with_priority`
    /// (or `enter_with_priority`) to name a lane.
    fn enter_armed<F: FnOnce()>(
        &self,
        arming: FaultRecord,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        self.enter_armed_with_priority(arming, Priority::Normal, rollback)
    }

    /// Shared admission core with an explicit priority lane (Phase 1).
    ///
    /// Same breaker → epoch → bulkhead → queue → commit pipeline; the
    /// lane is stamped on the guard (`TrackedGuard::priority`) and into
    /// the live `lane_in_flight` histogram (integers only, PHASE C).
    /// `Critical` over pure-`Background` occupancy files a cooperative
    /// preempt request (`preempt_requested`) instead of bouncing — the
    /// victims yield themselves via their own `TaskConduit` (R5).
    fn enter_armed_with_priority<F: FnOnce()>(
        &self,
        arming: FaultRecord,
        priority: Priority,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        let header = arming.header;
        let epoch = header.epoch;
        let task_id = header.task_id;
        let mut inner = self.lock();

        // 1. Breaker: OPEN fails fast; HALF_OPEN admits exactly one probe.
        // PHASE A (under lock): integers + Arc bump only — see record_locked.
        match inner.breaker {
            BreakerState::Open => {
                let record = FaultRecord {
                    kind: FaultKind::BreakerOpen,
                    ..arming.clone()
                };
                let mut outcome = Self::record_locked(&mut inner, FaultKind::BreakerOpen, 1, 0);
                drop(inner);
                self.publish_fault(&mut outcome, &record);
                Self::emit(outcome, record);
                return Err(EnterError::BreakerOpen);
            }
            BreakerState::HalfOpen => {
                if inner.half_open_probe {
                    let record = FaultRecord {
                        kind: FaultKind::BreakerOpen,
                        ..arming.clone()
                    };
                    let mut outcome = Self::record_locked(&mut inner, FaultKind::BreakerOpen, 1, 0);
                    drop(inner);
                    self.publish_fault(&mut outcome, &record);
                    Self::emit(outcome, record);
                    return Err(EnterError::BreakerOpen);
                }
                inner.half_open_probe = true;
            }
            BreakerState::Closed => {}
        }

        // 2. Epoch: stale packets never touch hardware.
        // F3 CONTRACT: HARD refusal, no hidden retry in-crate. The bounded
        // stale-retry loop lives ONLY in `tests/cycle_torture.rs` (storm
        // imitation). Smuggling a retry here turns fail-fast into a lot.
        if !inner.gate.observe(&header) {
            if inner.breaker == BreakerState::HalfOpen {
                inner.half_open_probe = false;
            }
            let record = FaultRecord {
                kind: FaultKind::StaleEpoch,
                ..arming.clone()
            };
            let mut outcome = Self::record_locked(&mut inner, FaultKind::StaleEpoch, 2, 0);
            drop(inner);
            self.publish_fault(&mut outcome, &record);
            Self::emit(outcome, record);
            return Err(EnterError::StaleEpoch);
        }

        // 3. Bulkhead: bounded slots, policy-driven overflow.
        // `BlockCaller(ms)` per-kind action parks the CALLER (lock
        // released!) up to `ms`, then re-checks once. Admit if a slot freed,
        // else reject as BulkheadFull. No condvar, no wakeups — the slot
        // frees when some OTHER thread settles its guard.
        // `BackpressurePolicy::AdaptiveBlock` (node level) does the same
        // through the friction function instead of a const: wait =
        // min(friction(in_flight, max), max_wait_ms), shed past depth cap.
        //
        // Phase 1 preemption (strict, cooperative, zero-deadlock): a
        // `Critical` admission over pure-`Background` occupancy files ONE
        // `Preempted` request (flag + counter, lock released!) and then
        // falls through to the normal `BulkheadFull` refusal — the CALLER
        // retries after its victims yield. `Critical` vs `Normal` never
        // preempts (no inversion). The node never waits holding the lock.
        if inner.in_flight >= inner.max_in_flight {
            if priority.may_preempt()
                && inner.in_flight > 0
                && inner.lane_in_flight[0] == inner.in_flight
                && !inner.preempt_requested
            {
                inner.preempt_requested = true;
                let mut outcome = Self::record_locked(&mut inner, FaultKind::Preempted, 7, 0);
                drop(inner);
                let record = FaultRecord {
                    kind: FaultKind::Preempted,
                    ..arming.clone()
                };
                self.publish_fault(&mut outcome, &record);
                Self::emit(outcome, record);
                inner = self.lock();
            }
            match inner.overflow {
                OverflowPolicy::RejectNew => {
                    // PHASE A snapshot: Copy out everything the friction math
                    // needs, then DROP the lock — the foreign `friction_fn`
                    // runs OUTSIDE (PHASE B). Foreign code under a lock =
                    // deadlock by definition + poison on panic.
                    let snap_in_flight = inner.in_flight;
                    let snap_max = inner.max_in_flight;
                    let snap_policy = inner.backpressure_policy;
                    let snap_friction = inner.friction_fn;
                    let snap_action = inner.policies[FaultKind::BulkheadFull.index()].action;
                    drop(inner);
                    // PHASE B (no lock): foreign + float math here.
                    let max = snap_max.max(1) as u64;
                    let depth = ((snap_in_flight as u64 * 255 / max).min(255)) as u8;
                    let cap = match snap_policy {
                        BackpressurePolicy::AdaptiveBlock { max_wait_ms, .. } => max_wait_ms.max(1),
                        _ => 1000,
                    };
                    let friction_wait = match snap_friction {
                        Some(f) => f(snap_in_flight as u32, snap_max as u32),
                        None => 1 + depth as u64 * cap / 255,
                    };
                    let legacy_ms = match snap_action {
                        FaultAction::BlockCaller(ms) => ms,
                        _ => 0,
                    };
                    let (park_ms, shed_now) = match snap_policy {
                        BackpressurePolicy::AdaptiveBlock {
                            max_wait_ms,
                            shed_deeper_than,
                        } => {
                            if shed_deeper_than.is_some_and(|d| depth > d) {
                                (0, true)
                            } else {
                                (friction_wait.min(max_wait_ms.max(1)), false)
                            }
                        }
                        BackpressurePolicy::Custom(_) => (0, false),
                        BackpressurePolicy::AdviseOnly => (0, false),
                    };
                    // PHASE C: re-lock for the verdict (fresh state!).
                    // Re-check under the FRESH lock: a slot may have freed
                    // while PHASE B ran (no TOCTOU — admit if free).
                    inner = self.lock();
                    if inner.in_flight < inner.max_in_flight {
                        // Freed while computing — fall through to admit.
                    } else {
                        let _wait_ms_inner = if legacy_ms > 0 { legacy_ms } else { park_ms };
                        if matches!(snap_policy, BackpressurePolicy::Custom(_)) {
                            let bp =
                                Backpressure::new(friction_wait, depth, FaultKind::BulkheadFull);
                            drop(inner);
                            if let BackpressurePolicy::Custom(react) = snap_policy {
                                react(bp);
                            }
                            inner = self.lock();
                            if inner.in_flight < inner.max_in_flight {
                                // Freed while reacting — admit.
                            } else {
                                if inner.breaker == BreakerState::HalfOpen {
                                    inner.half_open_probe = false;
                                }
                                let record = FaultRecord {
                                    kind: FaultKind::BulkheadFull,
                                    ..arming.clone()
                                };
                                let mut outcome =
                                    Self::record_locked(&mut inner, FaultKind::BulkheadFull, 3, 0);
                                drop(inner);
                                self.publish_fault(&mut outcome, &record);
                                Self::emit(outcome, record);
                                return Err(EnterError::BulkheadFull);
                            }
                        } else if shed_now {
                            // Load shedding: past the depth cap, no parking.
                            if inner.breaker == BreakerState::HalfOpen {
                                inner.half_open_probe = false;
                            }
                            let record = FaultRecord {
                                kind: FaultKind::BulkheadFull,
                                ..arming.clone()
                            };
                            let mut outcome =
                                Self::record_locked(&mut inner, FaultKind::BulkheadFull, 3, 0);
                            drop(inner);
                            self.publish_fault(&mut outcome, &record);
                            Self::emit(outcome, record);
                            return Err(EnterError::BulkheadFull);
                        }
                    }
                    let wait_ms = if legacy_ms > 0 { legacy_ms } else { park_ms };
                    if wait_ms > 0 {
                        drop(inner);
                        std::thread::sleep(core::time::Duration::from_millis(wait_ms));
                        inner = self.lock();
                        if inner.in_flight < inner.max_in_flight {
                            // Slot freed while parked — fall through to admit.
                        } else {
                            if inner.breaker == BreakerState::HalfOpen {
                                inner.half_open_probe = false;
                            }
                            let record = FaultRecord {
                                kind: FaultKind::BulkheadFull,
                                ..arming.clone()
                            };
                            let mut outcome =
                                Self::record_locked(&mut inner, FaultKind::BulkheadFull, 3, 0);
                            drop(inner);
                            self.publish_fault(&mut outcome, &record);
                            Self::emit(outcome, record);
                            return Err(EnterError::BulkheadFull);
                        }
                    } else {
                        if inner.breaker == BreakerState::HalfOpen {
                            inner.half_open_probe = false;
                        }
                        let record = FaultRecord {
                            kind: FaultKind::BulkheadFull,
                            ..arming.clone()
                        };
                        let mut outcome =
                            Self::record_locked(&mut inner, FaultKind::BulkheadFull, 3, 0);
                        drop(inner);
                        self.publish_fault(&mut outcome, &record);
                        Self::emit(outcome, record);
                        return Err(EnterError::BulkheadFull);
                    }
                }
                OverflowPolicy::EvictOldest => {
                    let record = FaultRecord {
                        kind: FaultKind::EvictedOldest,
                        ..arming.clone()
                    };
                    let mut outcome =
                        Self::record_locked(&mut inner, FaultKind::EvictedOldest, 4, 0);
                    drop(inner);
                    self.publish_fault(&mut outcome, &record);
                    Self::emit(outcome, record);
                    inner = self.lock();
                }
            }
        }

        // 4. Queue horizon: backpressure gauge when the buffer is full.
        if inner.queued >= inner.queue_limit {
            let record = FaultRecord {
                kind: FaultKind::BufferOverflow,
                ..arming.clone()
            };
            let mut outcome = Self::record_locked(&mut inner, FaultKind::BufferOverflow, 3, 0);
            drop(inner);
            self.publish_fault(&mut outcome, &record);
            Self::emit(outcome, record);
            inner = self.lock();
        }

        // 5. Token bucket: at most rate_per_sec admissions per second.
        if inner.rate_per_sec > 0 {
            let now = now_ms();
            let elapsed = now.saturating_sub(inner.rate_last_ms) as f64 / 1000.0;
            inner.rate_last_ms = now;
            inner.rate_tokens = (inner.rate_tokens + elapsed * inner.rate_per_sec as f64)
                .min(inner.rate_per_sec as f64);
            if inner.rate_tokens < 1.0 {
                if inner.breaker == BreakerState::HalfOpen {
                    inner.half_open_probe = false;
                }
                inner.rate_rejected = inner.rate_rejected.saturating_add(1);
                let rate_rejected = inner.rate_rejected;
                let record = FaultRecord {
                    kind: FaultKind::BulkheadFull,
                    ..arming.clone()
                };
                let mut outcome =
                    Self::record_locked(&mut inner, FaultKind::BulkheadFull, 6, rate_rejected);
                drop(inner);
                self.publish_fault(&mut outcome, &record);
                Self::emit(outcome, record);
                return Err(EnterError::RateLimited);
            }
            inner.rate_tokens -= 1.0;
            inner.stats.rate_rejected = inner.rate_rejected;
        }

        // PHASE C (commit): integers only, saturating, hard clamp.
        inner.in_flight = inner.in_flight.saturating_add(1);
        inner.queued = inner.queued.saturating_add(1);
        let lane = priority.code().min(2) as usize;
        inner.lane_in_flight[lane] = inner.lane_in_flight[lane].saturating_add(1);
        if priority == Priority::Critical {
            inner.preempt_requested = false;
        }
        inner.stats.total_started = inner.stats.total_started.saturating_add(1);
        hard_clamp_slots(&mut inner);
        let node_name = inner.name;
        let was_half_open = inner.breaker == BreakerState::HalfOpen;
        let deadline = inner.timeout_ms;
        drop(inner);

        Ok(TrackedGuard {
            node: self.inner.clone(),
            node_name,
            guard: Some(super::SnubberGuard::new(epoch, task_id, rollback)),
            arming,
            settled: false,
            is_probe: was_half_open,
            priority,
            armed_at_ms: now_ms(),
            deadline_ms: deadline,
            stage: 0,
            stage_label: None,
            queue_wait_ms: 0,
            retry_count: 0,
        })
    }

    /// Manually close the breaker after an external recovery.
    pub fn reset_breaker(&self) {
        let mut inner = self.lock();
        inner.breaker = BreakerState::Closed;
        inner.half_open_probe = false;
        inner.consecutive_failures = 0;
        inner.stats.breaker_state = BreakerState::Closed;
    }

    /// Compile-time error budget: bake `MAX_ERRORS` into the call site.
    ///
    /// Returns a builder with the console reporter armed for at most
    /// `MAX_ERRORS` lines per node lifetime. The budget is a const
    /// generic — `SnubberNode::<8>::capped("drv")` vs
    /// `SnubberNode::<0>::capped("drv")` (silent) are different types
    /// of intent, checked at compile time, with zero runtime cost.
    ///
    /// When `MAX_ERRORS == 0` the node still records hooks + `stats()`,
    /// it just never prints.
    pub fn with_error_budget<const MAX_ERRORS: usize>(name: &'static str) -> SnubberNodeBuilder {
        SnubberNode::builder(name)
            .console_report(MAX_ERRORS > 0)
            .console_cap(MAX_ERRORS)
    }

    /// Console reporter with auto-detect: prints only when stderr is a
    /// real console (desktop daemon / CLI session), never into a pipe,
    /// file redirect, or a GUI/service process without a terminal.
    ///
    /// Detection is one [`std::io::IsTerminal`] probe at build time
    /// (stable since Rust 1.70, our MSRV — no FFI, no `unsafe`), so there
    /// is no per-fault syscall. Combine with
    /// [`SnubberNode::with_error_budget`] for the cap:
    ///
    /// ```rust,no_run
    /// use snubber::SnubberNode;
    /// let node = SnubberNode::console_node::<8>("win32_mouse_driver");
    /// ```
    pub fn console_node<const MAX_ERRORS: usize>(name: &'static str) -> SnubberNode {
        Self::with_error_budget::<MAX_ERRORS>(name)
            .console_report(self::console_attached())
            .build()
    }

    /// Bulkhead probe: `true` if `in_flight < max_in_flight` right now.
    #[must_use]
    pub fn bulkhead_has_room(&self) -> bool {
        let inner = self.lock();
        inner.in_flight < inner.max_in_flight
    }

    /// Single atomic admission attempt for `enter_async`:
    /// Checks breaker, epoch gate, bulkhead, and token bucket.
    /// If bulkhead is full, parks `waker` in `inner.waiters` and returns `Pending`
    /// WITHOUT consuming `rollback`!
    pub(crate) fn enter_async_attempt<F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        loc: &'static core::panic::Location<'static>,
        rollback: &mut Option<F>,
        waker: &core::task::Waker,
        enqueued_at_ms: u64,
    ) -> core::task::Poll<Result<TrackedGuard<F>, EnterError>> {
        let now = now_ms();
        let wait_ms = now.saturating_sub(enqueued_at_ms);
        let header = TransactionHeader::new(epoch, task_id);
        let mut inner = self.lock();

        // 0. Task TTL check: if task waited longer than task_ttl_ms, fail fast.
        if inner.task_ttl_ms > 0 && wait_ms > inner.task_ttl_ms {
            let record = FaultRecord {
                kind: FaultKind::Timeout,
                node: inner.name,
                file: loc.file(),
                line: loc.line(),
                header,
                detail: Some(FaultDetail::Static("task ttl expired in queue")),
                stage: 0,
                stage_label: None,
                queue_wait_ms: wait_ms,
                elapsed_ms: 0,
                retry_count: 0,
            };
            let mut outcome = Self::record_locked(&mut inner, FaultKind::Timeout, 0, 0);
            drop(inner);
            self.publish_fault(&mut outcome, &record);
            Self::emit(outcome, record);
            return core::task::Poll::Ready(Err(EnterError::QueueTtlExpired));
        }

        // 1. Breaker: OPEN fails fast; HALF_OPEN admits exactly one probe.
        match inner.breaker {
            BreakerState::Open => {
                let record = FaultRecord {
                    kind: FaultKind::BreakerOpen,
                    node: inner.name,
                    file: loc.file(),
                    line: loc.line(),
                    header,
                    detail: None,
                    stage: 0,
                    stage_label: None,
                    queue_wait_ms: wait_ms,
                    elapsed_ms: 0,
                    retry_count: 0,
                };
                let mut outcome = Self::record_locked(&mut inner, FaultKind::BreakerOpen, 1, 0);
                drop(inner);
                self.publish_fault(&mut outcome, &record);
                Self::emit(outcome, record);
                return core::task::Poll::Ready(Err(EnterError::BreakerOpen));
            }
            BreakerState::HalfOpen => {
                if inner.half_open_probe {
                    let record = FaultRecord {
                        kind: FaultKind::BreakerOpen,
                        node: inner.name,
                        file: loc.file(),
                        line: loc.line(),
                        header,
                        detail: None,
                        stage: 0,
                        stage_label: None,
                        queue_wait_ms: wait_ms,
                        elapsed_ms: 0,
                        retry_count: 0,
                    };
                    let mut outcome = Self::record_locked(&mut inner, FaultKind::BreakerOpen, 1, 0);
                    drop(inner);
                    self.publish_fault(&mut outcome, &record);
                    Self::emit(outcome, record);
                    return core::task::Poll::Ready(Err(EnterError::BreakerOpen));
                }
                inner.half_open_probe = true;
            }
            BreakerState::Closed => {}
        }

        // 2. Epoch: stale packets never touch hardware.
        let is_fresh = if header.generation > inner.gate.generation() {
            header.epoch > 0
        } else if header.generation < inner.gate.generation() {
            false
        } else {
            header.epoch > inner.gate.last_seen()
        };
        if !is_fresh {
            if inner.breaker == BreakerState::HalfOpen {
                inner.half_open_probe = false;
            }
            let record = FaultRecord {
                kind: FaultKind::StaleEpoch,
                node: inner.name,
                file: loc.file(),
                line: loc.line(),
                header,
                detail: None,
                stage: 0,
                stage_label: None,
                queue_wait_ms: wait_ms,
                elapsed_ms: 0,
                retry_count: 0,
            };
            let mut outcome = Self::record_locked(&mut inner, FaultKind::StaleEpoch, 2, 0);
            drop(inner);
            self.publish_fault(&mut outcome, &record);
            Self::emit(outcome, record);
            return core::task::Poll::Ready(Err(EnterError::StaleEpoch));
        }

        // 3. Bulkhead: if full, park the waker and return Pending WITHOUT consuming rollback!
        if inner.in_flight >= inner.max_in_flight {
            if inner.breaker == BreakerState::HalfOpen {
                inner.half_open_probe = false;
            }
            let q_cap = inner.queue_limit.max(1);
            if inner.waiters.len() >= q_cap {
                match inner.overflow {
                    OverflowPolicy::RejectNew => {
                        let record = FaultRecord {
                            kind: FaultKind::BufferOverflow,
                            node: inner.name,
                            file: loc.file(),
                            line: loc.line(),
                            header,
                            detail: Some(FaultDetail::Static("wait queue full")),
                            stage: 0,
                            stage_label: None,
                            queue_wait_ms: wait_ms,
                            elapsed_ms: 0,
                            retry_count: 0,
                        };
                        let mut outcome =
                            Self::record_locked(&mut inner, FaultKind::BufferOverflow, 3, 0);
                        drop(inner);
                        self.publish_fault(&mut outcome, &record);
                        Self::emit(outcome, record);
                        return core::task::Poll::Ready(Err(EnterError::QueueFull));
                    }
                    OverflowPolicy::EvictOldest => {
                        inner.waiters.pop_front();
                        inner.stats.evicted = inner.stats.evicted.saturating_add(1);
                    }
                }
            }
            inner.waiters.push_back(waker.clone());
            return core::task::Poll::Pending;
        }

        // 4. Token bucket: at most rate_per_sec admissions per second.
        if inner.rate_per_sec > 0 {
            let elapsed = now.saturating_sub(inner.rate_last_ms) as f64 / 1000.0;
            inner.rate_last_ms = now;
            inner.rate_tokens = (inner.rate_tokens + elapsed * inner.rate_per_sec as f64)
                .min(inner.rate_per_sec as f64);
            if inner.rate_tokens < 1.0 {
                if inner.breaker == BreakerState::HalfOpen {
                    inner.half_open_probe = false;
                }
                inner.rate_rejected = inner.rate_rejected.saturating_add(1);
                let rate_rejected = inner.rate_rejected;
                let record = FaultRecord {
                    kind: FaultKind::BulkheadFull,
                    node: inner.name,
                    file: loc.file(),
                    line: loc.line(),
                    header,
                    detail: None,
                    stage: 0,
                    stage_label: None,
                    queue_wait_ms: wait_ms,
                    elapsed_ms: 0,
                    retry_count: 0,
                };
                let mut outcome =
                    Self::record_locked(&mut inner, FaultKind::BulkheadFull, 6, rate_rejected);
                drop(inner);
                self.publish_fault(&mut outcome, &record);
                Self::emit(outcome, record);
                return core::task::Poll::Ready(Err(EnterError::RateLimited));
            }
            inner.rate_tokens -= 1.0;
            inner.stats.rate_rejected = inner.rate_rejected;
        }

        // 5. Commit slot:
        inner.gate.observe(&header);
        inner.in_flight = inner.in_flight.saturating_add(1);
        inner.queued = inner.queued.saturating_add(1);
        inner.lane_in_flight[1] = inner.lane_in_flight[1].saturating_add(1);
        inner.stats.total_started = inner.stats.total_started.saturating_add(1);
        hard_clamp_slots(&mut inner);

        let is_probe = inner.breaker == BreakerState::HalfOpen;
        let deadline_ms = inner.timeout_ms;
        let node_arc = self.inner.clone();
        let node_name = inner.name;
        drop(inner);

        let rb = rollback.take().expect("rollback present on admission");
        let guard = super::SnubberGuard::new(epoch, task_id, rb);
        let arming = FaultRecord {
            kind: FaultKind::ImplicitDrop,
            node: node_name,
            file: loc.file(),
            line: loc.line(),
            header,
            detail: None,
            stage: 0,
            stage_label: None,
            queue_wait_ms: wait_ms,
            elapsed_ms: 0,
            retry_count: 0,
        };
        core::task::Poll::Ready(Ok(TrackedGuard {
            guard: Some(guard),
            node: node_arc,
            node_name,
            arming,
            deadline_ms,
            armed_at_ms: now,
            settled: false,
            is_probe,
            priority: Priority::Normal,
            stage: 0,
            stage_label: None,
            queue_wait_ms: wait_ms,
            retry_count: 0,
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NodeInner> {
        // `into_inner` recovers from a poisoned mutex (foreign hook panicked
        // AFTER the lock was released — the books are intact, the guard is
        // gone). ZERO-PANIC phases A/C make this path unreachable in theory;
        // in practice count it so `poison_recoveries > 0` screams in stats.
        match self.inner.lock() {
            Ok(g) => g,
            Err(poison) => {
                let mut g = poison.into_inner();
                g.stats.poison_recoveries = g.stats.poison_recoveries.saturating_add(1);
                // A torn write under panic is repaired, not trusted: clamp
                // slots to the hard ceiling so the breaker never reads rot.
                hard_clamp_slots(&mut g);
                g
            }
        }
    }

    /// Snap a stats copy WITHOUT cloning the fault record (ZERO-PANIC:
    /// field-wise `Copy` moves only — no `Clone` of `Option<FaultRecord>`
    /// which may own an `alloc::String` and allocate on clone).
    /// The caller attaches `last_fault` OUTSIDE the lock from the owned
    /// record it already holds.
    fn snapshot_stats_no_alloc(inner: &NodeInner) -> SnubberStats {
        let s = &inner.stats;
        SnubberStats {
            total_started: s.total_started,
            committed: s.committed,
            explicit_aborts: s.explicit_aborts,
            poisoned_rejected: s.poisoned_rejected,
            implicit_drops: s.implicit_drops,
            stale_epochs_rejected: s.stale_epochs_rejected,
            bulkhead_rejections: s.bulkhead_rejections,
            evicted: s.evicted,
            preempted: s.preempted,
            breaker_rejections: s.breaker_rejections,
            breaker_trips: s.breaker_trips,
            breaker_state: s.breaker_state,
            timeouts_fired: s.timeouts_fired,
            rate_rejected: s.rate_rejected,
            window_outcomes: s.window_outcomes,
            window_failures: s.window_failures,
            poison_recoveries: s.poison_recoveries,
            last_fault: None,
            reported: s.reported,
        }
    }

    /// Second half of `record_locked` for explicit `report()` paths.
    /// Integer-only, saturating, cannot panic — hard invariant, never
    /// `debug_assert` (survives `--release`).
    #[inline]
    fn bump_reported(inner: &mut NodeInner, kind: FaultKind) {
        inner.stats.reported[kind.index()] = inner.stats.reported[kind.index()].saturating_add(1);
        match kind {
            FaultKind::ImplicitDrop => {
                inner.stats.implicit_drops = inner.stats.implicit_drops.saturating_add(1);
            }
            FaultKind::StaleEpoch => {
                inner.stats.stale_epochs_rejected =
                    inner.stats.stale_epochs_rejected.saturating_add(1);
            }
            FaultKind::BulkheadFull | FaultKind::BufferOverflow => {
                inner.stats.bulkhead_rejections = inner.stats.bulkhead_rejections.saturating_add(1);
            }
            FaultKind::EvictedOldest => {
                inner.stats.evicted = inner.stats.evicted.saturating_add(1);
            }
            FaultKind::BreakerOpen => {
                inner.stats.breaker_rejections = inner.stats.breaker_rejections.saturating_add(1);
            }
            FaultKind::Preempted => {
                inner.stats.preempted = inner.stats.preempted.saturating_add(1);
            }
            _ => {}
        }
    }

    /// Store fault + bump counters + apply the per-kind policy, snapshot,
    /// clone hooks — then the CALLER must drop the lock before invoking
    /// anything. Returns the owned data the caller reports without
    /// holding the mutex.
    ///
    /// ZERO-PANIC CONTRACT (phases A/C): under the lock ONLY `Copy`
    /// moves, integer `saturating_*` math, flag flips and one `Arc`
    /// refcount bump. NO `format!`, NO `String`, NO `Vec::clone`, NO
    /// `stats.clone()` (owns `last_fault` → may allocate), NO
    /// `record.clone()` (may own `FaultDetail::Owned` → allocates),
    /// NO foreign calls (`friction_fn`, hooks, custom). Everything that
    /// can allocate or panic is computed OUTSIDE the lock from the
    /// primitive snapshot the lock hands out.
    ///
    /// `bump_kind` is a plain integer code (not a closure over caller
    /// state) so the signature itself cannot smuggle a panicking call
    /// under the lock: 0 = none, 1 = breaker_rejections,
    /// 2 = stale_epochs_rejected, 3 = bulkhead_rejections, 4 = evicted,
    /// 5 = poisoned_rejected (+reported), 6 = rate mirror
    /// (`extra` = rate_rejected value), 7 = preempted (+reported).
    fn record_locked(
        inner: &mut NodeInner,
        record_kind: FaultKind,
        bump_kind: u8,
        extra: u64,
    ) -> ReportOutcome {
        let policy = inner.policies[record_kind.index()];
        match bump_kind {
            1 => {
                inner.stats.breaker_rejections = inner.stats.breaker_rejections.saturating_add(1);
            }
            2 => {
                inner.stats.stale_epochs_rejected =
                    inner.stats.stale_epochs_rejected.saturating_add(1);
            }
            3 => {
                inner.stats.bulkhead_rejections = inner.stats.bulkhead_rejections.saturating_add(1);
            }
            4 => {
                inner.stats.evicted = inner.stats.evicted.saturating_add(1);
            }
            5 => {
                inner.stats.poisoned_rejected = inner.stats.poisoned_rejected.saturating_add(1);
                inner.stats.reported[record_kind.index()] =
                    inner.stats.reported[record_kind.index()].saturating_add(1);
            }
            6 => {
                inner.stats.bulkhead_rejections = inner.stats.bulkhead_rejections.saturating_add(1);
                inner.stats.rate_rejected = extra;
            }
            7 => {
                inner.stats.preempted = inner.stats.preempted.saturating_add(1);
                inner.stats.reported[record_kind.index()] =
                    inner.stats.reported[record_kind.index()].saturating_add(1);
            }
            _ => {}
        }
        // Flight ring: Copy codes only (kind + epoch/task + caller coords
        // come from the CALLER-built record, not from here — record_locked
        // only sees codes, so report()/observe() push the slot explicitly).
        // This spot stays integer-only: no push here, no alloc, no panic.
        // Policy COMMAND half (under lock, no calls out).
        // `BlockCaller` never reaches here as a command: it is consumed at
        // admission time (enter_inner parks the caller); if a fault FIRES
        // while BlockCaller is armed, the firing itself is silent.
        let custom: Option<fn()> = match policy.action {
            FaultAction::DiscardSilently
            | FaultAction::BreakerOpen
            | FaultAction::BlockCaller(_) => None,
            FaultAction::RollbackAndAlert => {
                inner.alert_pending = true;
                None
            }
            FaultAction::TripBreaker => {
                // Policy trip honors the OFF switch: `trip_after_failures(0)`
                // + `failure_window(0)` = breaker fully disabled (heavy stress /
                // server nodes). A policy that trips while disabled would
                // isolate a node that asked for no isolation — the "1 of
                // 40000" trap: 416 Panic reports with default TripBreaker
                // policy weld the breaker OPEN after the first report.
                let streak_off = inner.trip_after_failures == 0;
                let window_off = inner.failure_window == 0;
                if inner.breaker != BreakerState::HalfOpen && !(streak_off && window_off) {
                    inner.breaker = BreakerState::Open;
                    inner.stats.breaker_trips = inner.stats.breaker_trips.saturating_add(1);
                    inner.stats.breaker_state = BreakerState::Open;
                }
                None
            }
            FaultAction::Custom(cmd) => Some(cmd),
        };
        let snapshot = Self::snapshot_stats_no_alloc(inner);
        let hooks = Arc::clone(&inner.hooks);
        let frame = inner.console_frame;
        // Console budget: reserve integers under the lock, format OUTSIDE.
        // Single arm — (Some, Some) and (Some, None) differ only in text,
        // which is built in PHASE B anyway. Two arms was F9 duplication.
        let console_slot: Option<(u32, usize, usize)> = match (policy.log_level, policy.log_msg) {
            (Some(level), _) if inner.console && inner.console_used < inner.console_cap => {
                inner.console_used = inner.console_used.saturating_add(1);
                Some((level_as_u32(level), inner.console_used, inner.console_cap))
            }
            _ => None,
        };
        ReportOutcome {
            hooks,
            snapshot,
            policy,
            custom,
            console_slot,
            frame,
            line: None,
        }
    }

    fn emit(mut outcome: ReportOutcome, record: FaultRecord) {
        // PHASE B (no lock): format console + clone record OUTSIDE.
        if let Some((level_u32, used, cap)) = outcome.console_slot {
            let level = level_from_u32(level_u32);
            if outcome.frame {
                outcome.line = Some(format!(
                    "[SNUB:ALERT] node=\"{}\" kind={:?} count={} depth={} file={} line={}",
                    record.node,
                    record.kind,
                    outcome.snapshot.reported[record.kind.index()],
                    outcome.snapshot.in_flight_depth(),
                    record.file,
                    record.line,
                ));
            } else {
                let text = outcome.policy.log_msg.unwrap_or("fault");
                let detail_owned: Option<String> =
                    record.detail.as_ref().map(|d| d.detail().to_string());
                let detail = match &detail_owned {
                    Some(d) => format!(" — {d}"),
                    None => String::new(),
                };
                outcome.line = Some(format!(
                    "[snubber:{}][{:?}] {:?} at {}:{} ({}) — {text}{detail} [used {used}/{cap}]",
                    record.node, level, record.kind, record.file, record.line, record.header,
                ));
            }
        }
        // Policy COMMAND half, outside the lock: self-healing runs here.
        // Isolation lives in `crate::watchdog::isolate_once`: a panic in one
        // hook must not silence remaining observers (or the console
        // line after it). Under `std` each call gets its own panic
        // boundary; without `std` the contract is "hooks never panic".
        //
        // Unwind note: PHASE A/C above cannot panic (integers + Arc bump
        // only), so a panic here unwinds with the books already settled
        // and the caller's `Drop` chain (outer meta-guard) fires normally
        // during unwinding — hardware is released, no deadlock.
        if let Some(cmd) = outcome.custom {
            let _ = crate::watchdog::isolate_once(cmd);
        }
        for hook in outcome.hooks.iter() {
            let record = &record;
            let snapshot = &outcome.snapshot;
            let policy = &outcome.policy;
            let _ = crate::watchdog::isolate_once(|| {
                hook(record, snapshot, policy);
            });
        }
        if let Some(line) = outcome.line {
            eprintln!("{line}");
        }
    }
}

/// Free helper for `TrackedGuard` paths (no `self: SnubberNode` there —
/// only `Arc<Mutex<NodeInner>>`). Same ZERO-PANIC rule: clones BEFORE
/// the second lock, pointer moves under it.
fn publish_fault_arc(
    node: &Arc<Mutex<NodeInner>>,
    outcome: &mut ReportOutcome,
    record: &FaultRecord,
) {
    outcome.snapshot.last_fault = Some(record.clone());
    let owned = record.clone();
    let slot = FlightSlot {
        kind: record.kind,
        epoch: record.header.epoch,
        task_id: record.header.task_id,
        file: record.file,
        line: record.line,
    };
    {
        let mut inner = node.lock().unwrap_or_else(|p| {
            let mut g = p.into_inner();
            g.stats.poison_recoveries = g.stats.poison_recoveries.saturating_add(1);
            hard_clamp_slots(&mut g);
            g
        });
        inner.stats.last_fault = Some(owned);
        push_flight_inner(&mut inner, slot);
    }
}

/// Push one flight slot with oldest-evict at cap. `VecDeque` was
/// preallocated at `build()`; at cap we `pop_front` (no realloc).
/// `Copy`-only payload — cannot panic, PHASE-C safe. Free shape (not a
/// method) so `publish_fault_arc` can use it without a node handle.
fn push_flight_inner(inner: &mut NodeInner, slot: FlightSlot) {
    if inner.flight.len() >= inner.flight_depth.max(1) {
        inner.flight.pop_front();
    }
    // At cap the deque is full but never over: push_back on a
    // with_capacity buffer reuses the freed front slot. If a poisoned
    // recovery ever shrank it, push may realloc once — cold path only
    // (publish, never admission), counted via lock() like stats().
    inner.flight.push_back(slot);
    // Hard clamp: depth knob is a promise, not a hint.
    while inner.flight.len() > inner.flight_depth.max(1) {
        inner.flight.pop_front();
    }
}

/// Release one armed slot on a settle path (Phase 1 preemption books).
///
/// Integers only, cannot panic: `in_flight`/`queued` come home exactly
/// once, the guard's lane is subtracted from the live histogram, and a
/// settling victim clears a pending cooperative `preempt_requested`.
/// Every `commit` / `abort` / `check_timeout` / `settle_implicit` funnels
/// here — one owner, no drift, no double-bookkeeping.
#[inline]
fn release_slot_inner(inner: &mut NodeInner, lane: usize) {
    inner.in_flight = inner.in_flight.saturating_sub(1);
    inner.queued = inner.queued.saturating_sub(1);
    let lane = lane.min(2);
    inner.lane_in_flight[lane] = inner.lane_in_flight[lane].saturating_sub(1);
    if lane == 0 {
        inner.preempt_requested = false;
    }
    hard_clamp_slots(inner);
}
/// Owned fallout of [`SnubberNode::record_locked`]: everything the caller
/// reports WITHOUT holding the node lock (hooks, console, self-heal).
/// Named type instead of a 5-tuple (clippy `type_complexity`).
///
/// ZERO-PANIC note: `console_slot` reserves the console budget under the
/// lock as plain integers; the `String` is formatted later in
/// `finish_report` with NO lock held.
struct ReportOutcome {
    hooks: Arc<Vec<FaultHook>>,
    snapshot: SnubberStats,
    policy: FaultPolicy,
    custom: Option<fn()>,
    console_slot: Option<(u32, usize, usize)>,
    frame: bool,
    line: Option<String>,
}

/// `LogLevel` as a `Copy` integer for the lock-free handoff.
#[inline]
fn level_as_u32(level: LogLevel) -> u32 {
    match level {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warn => 2,
        LogLevel::Error => 3,
        LogLevel::Critical => 4,
    }
}

/// Inverse of [`level_as_u32`] (PHASE B, no lock).
#[inline]
fn level_from_u32(v: u32) -> LogLevel {
    match v {
        0 => LogLevel::Debug,
        1 => LogLevel::Info,
        2 => LogLevel::Warn,
        4 => LogLevel::Critical,
        _ => LogLevel::Error,
    }
}
/// Guard armed through a [`SnubberNode`]: settles the black box on exit.
///
/// * `commit()` → `committed += 1`, breaker failure streak reset
///   (a probe success closes the breaker).
/// * `abort(reason)` → `explicit_aborts += 1` (planned, no breaker trip).
/// * Implicit `Drop` → `implicit_drops += 1` + `FaultRecord` with the
///   ARMING coordinates, breaker streak +1, trip at the threshold.
/// * `check_timeout()` → cooperative watchdog: if the per-guard deadline
///   (see [`SnubberNodeBuilder::timeout_ms`]) passed, rolls back NOW,
///   fires the `Timeout` policy and returns `Err(Timeout)`.
///
/// Honest contract on hanging: a thread blocked INSIDE a Win32 call
/// cannot be preempted — `check_timeout` fires at the points the worker
/// CAN reach (loop iterations, progress emits). The node never spawns a
/// killer thread behind your back.
#[must_use = "a TrackedGuard that is never committed rolls back on drop"]
pub struct TrackedGuard<F: FnOnce()> {
    node: Arc<Mutex<NodeInner>>,
    node_name: &'static str,
    guard: Option<super::SnubberGuard<F>>,
    arming: FaultRecord,
    settled: bool,
    is_probe: bool,
    priority: Priority,
    armed_at_ms: u64,
    deadline_ms: u64,
    stage: u32,
    stage_label: Option<&'static str>,
    queue_wait_ms: u64,
    retry_count: u32,
}

/// Watchdog outcome for [`TrackedGuard::check_timeout`].
#[must_use = "a TimedOut guard already settled (rollback ran) — propagate, never keep working"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutCheck {
    /// Within budget (or no deadline armed) — keep working.
    WithinBudget,
    /// Deadline passed: rollback RAN, `Timeout` policy fired.
    TimedOut,
}

impl<F: FnOnce()> TrackedGuard<F> {
    /// Has this guard passed its watchdog deadline?
    /// Returns `false` if no timeout was configured (`deadline_ms == 0`) or if already settled.
    #[inline]
    #[must_use]
    pub fn is_expired(&self) -> bool {
        if self.settled || self.deadline_ms == 0 {
            false
        } else {
            now_ms().saturating_sub(self.armed_at_ms) > self.deadline_ms
        }
    }

    /// Mark the pipeline stage reached (checkpoint for the reaper).
    ///
    /// Survives `select!` kill: when the future is cancelled mid-`.await`,
    /// the implicit-drop record carries the last `stage`, so the reaper
    /// knows what to undo (stage 1 → undo step 1 only, not the world).
    /// Monotonic use expected (`0 → 1 → 2…`); overwrites are caller's bug.
    #[inline]
    pub fn set_stage(&mut self, stage: u32) {
        self.stage = stage;
    }

    /// Current checkpoint (0 = no checkpoint yet).
    #[inline]
    #[must_use]
    pub fn stage(&self) -> u32 {
        self.stage
    }

    /// Human-readable label of the last checkpoint (`None` if no checkpoint set).
    #[inline]
    #[must_use]
    pub fn stage_label(&self) -> Option<&'static str> {
        self.stage_label
    }

    /// Set stage + label in one call (combined [`set_stage`](Self::set_stage)).
    ///
    /// ```text
    /// guard.checkpoint(1, "connected");
    /// guard.checkpoint(2, "headers_sent");
    /// ```
    ///
    /// Survives `select!` kill: the implicit-drop record carries both the
    /// numeric stage AND the label, so the reaper/supervisor knows exactly
    /// what step to undo.
    #[inline]
    pub fn checkpoint(&mut self, stage: u32, label: &'static str) {
        self.stage = stage;
        self.stage_label = Some(label);
    }

    /// Milliseconds spent waiting in the queue before admission (0 for sync enter).
    #[inline]
    #[must_use]
    pub fn queue_wait_ms(&self) -> u64 {
        self.queue_wait_ms
    }

    /// Number of retries already performed for this logical task.
    #[inline]
    #[must_use]
    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }

    /// Set the retry count on this guard (e.g. from an outer retry loop / reconciliation queue).
    #[inline]
    pub fn set_retry_count(&mut self, retries: u32) {
        self.retry_count = retries;
    }

    /// Fluent builder: set the retry count on this guard.
    #[inline]
    pub fn with_retry_count(mut self, retries: u32) -> Self {
        self.retry_count = retries;
        self
    }

    /// Copy-snapshot of the guard's current state (stage, timing, deadline, retries).
    /// No lock, no allocation — all fields are `Copy`.
    #[must_use]
    pub fn state_snapshot(&self) -> GuardSnapshot {
        GuardSnapshot {
            header: self.arming.header,
            stage: self.stage,
            stage_label: self.stage_label,
            queue_wait_ms: self.queue_wait_ms,
            elapsed_ms: now_ms().saturating_sub(self.armed_at_ms),
            deadline_ms: self.deadline_ms,
            retry_count: self.retry_count,
        }
    }

    /// Settle asynchronously: run `commit_fut` across `.await`, then `commit()`
    /// on `Ok` or `abort("async_settle_failed")` on `Err`.
    ///
    /// The guard holds NO lock across `.await` points — it commits only on success.
    pub async fn settle_async<Fut, T, E>(self, commit_fut: Fut) -> Result<T, E>
    where
        Fut: core::future::Future<Output = Result<T, E>>,
    {
        crate::async_settle::settle_async(self, "async_settle_failed", commit_fut).await
    }

    /// Settle asynchronously with an explicit abort reason on error.
    pub async fn settle_async_with_reason<Fut, T, E>(
        self,
        reason: &'static str,
        commit_fut: Fut,
    ) -> Result<T, E>
    where
        Fut: core::future::Future<Output = Result<T, E>>,
    {
        crate::async_settle::settle_async(self, reason, commit_fut).await
    }

    /// Arming header (stamped at `enter`).
    #[must_use]
    pub fn header(&self) -> TransactionHeader {
        self.arming.header
    }

    /// Priority lane this guard was admitted on (Phase 1 preemption).
    /// `Background` guards are the ones a `Critical` admission may ask
    /// to yield via `Directive::Preempted` on their own `TaskConduit`.
    #[must_use]
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// Arming coordinates (the `enter` call site).
    #[must_use]
    pub fn arming_site(&self) -> (&'static str, u32) {
        (self.arming.file, self.arming.line)
    }

    /// Emit `Ack` for this transaction.
    pub fn ack<T>(&self) -> SnubberEvent<T> {
        SnubberEvent::Ack {
            header: self.arming.header,
        }
    }

    /// Emit `Progress` carrying a live payload.
    pub fn progress<T>(&self, payload: T) -> SnubberEvent<T> {
        SnubberEvent::Progress {
            header: self.arming.header,
            payload,
        }
    }

    /// Success path: hardware rollback disarmed, books settled.
    /// Wakes ONE parked `enter_async` waiter (outside the lock, PHASE B).
    pub fn commit(mut self) -> SnubberEvent<()> {
        let guard = self.guard.take().expect("guard already settled");
        let event = guard.commit();
        self.settled = true;
        let node = self.node.clone();
        let is_probe = self.is_probe;
        let lane = self.priority.code().min(2) as usize;
        core::mem::forget(self);
        let waker = {
            let mut inner = node.lock().unwrap_or_else(|p| {
                let mut g = p.into_inner();
                g.stats.poison_recoveries = g.stats.poison_recoveries.saturating_add(1);
                hard_clamp_slots(&mut g);
                g
            });
            inner.stats.committed = inner.stats.committed.saturating_add(1);
            Self::note_outcome(&mut inner, false);
            inner.stats.window_outcomes = inner.window_outcomes;
            inner.stats.window_failures = inner.window_failures;
            if is_probe {
                inner.breaker = BreakerState::Closed;
                inner.half_open_probe = false;
                inner.stats.breaker_state = BreakerState::Closed;
            }
            release_slot_inner(&mut inner, lane);
            inner.waiters.pop_front()
        };
        if let Some(w) = waker {
            w.wake();
        }
        event
    }

    /// Cooperative watchdog: call at every loop iteration / progress emit.
    ///
    /// * No deadline armed (`timeout_ms = 0`) → `WithinBudget`, zero cost
    ///   beyond one clock read.
    /// * Within budget → `WithinBudget`.
    /// * Deadline passed → hardware rollback runs NOW (guard consumed),
    ///   `Timeout` policy fires with the CHECK coordinates, streak fed,
    ///   `Err(TimeoutCheck::TimedOut)` returned. The caller MUST stop
    ///   working and propagate — the transaction is already settled.
    pub fn check_timeout(mut self) -> Result<Self, TimeoutCheck> {
        if self.settled {
            return Ok(self);
        }
        if self.deadline_ms == 0 || now_ms().saturating_sub(self.armed_at_ms) <= self.deadline_ms {
            return Ok(self);
        }
        // Deadline blown: settle as Timeout (rollback + policy + streak).
        self.settled = true;
        let loc = core::panic::Location::caller();
        let header = self.arming.header;
        let node_name = self.arming.node;
        let lane = self.priority.code().min(2) as usize;
        if let Some(g) = self.guard.take() {
            drop(g); // hardware rollback runs HERE
        }
        let mut inner = self.node.lock().unwrap_or_else(|p| {
            let mut g = p.into_inner();
            g.stats.poison_recoveries = g.stats.poison_recoveries.saturating_add(1);
            hard_clamp_slots(&mut g);
            g
        });
        release_slot_inner(&mut inner, lane);
        inner.timeouts_fired = inner.timeouts_fired.saturating_add(1);
        inner.stats.reported[FaultKind::Timeout.index()] =
            inner.stats.reported[FaultKind::Timeout.index()].saturating_add(1);
        Self::note_outcome(&mut inner, true);
        inner.stats.timeouts_fired = inner.timeouts_fired;
        inner.stats.window_outcomes = inner.window_outcomes;
        inner.stats.window_failures = inner.window_failures;
        let record = FaultRecord {
            kind: FaultKind::Timeout,
            node: node_name,
            file: loc.file(),
            line: loc.line(),
            header,
            detail: Some(FaultDetail::Static("deadline exceeded")),
            stage: self.stage,
            stage_label: self.stage_label,
            queue_wait_ms: self.queue_wait_ms,
            elapsed_ms: now_ms().saturating_sub(self.armed_at_ms),
            retry_count: self.retry_count,
        };
        let mut outcome = SnubberNode::record_locked(&mut inner, FaultKind::Timeout, 0, 0);
        let waker = inner.waiters.pop_front();
        drop(inner);
        publish_fault_arc(&self.node, &mut outcome, &record);
        SnubberNode::emit(outcome, record);
        if let Some(w) = waker {
            w.wake();
        }
        core::mem::forget(self);
        Err(TimeoutCheck::TimedOut)
    }

    /// Feed BOTH breaker modes from one settled outcome: reset the
    /// consecutive streak on success, advance it + the sliding window
    /// on failure, trip by whichever mode fires first.
    fn note_outcome(inner: &mut NodeInner, failed: bool) {
        // Sliding window always observes (when armed).
        if inner.failure_window > 0 {
            inner.window_outcomes = inner.window_outcomes.saturating_add(1);
            if failed {
                inner.window_failures = inner.window_failures.saturating_add(1);
            }
            if inner.window_outcomes >= inner.failure_window as u64 {
                let pct = inner.window_failures * 100 / inner.window_outcomes.max(1);
                if pct >= inner.failure_threshold_percent as u64
                    && inner.breaker == BreakerState::Closed
                {
                    inner.breaker = BreakerState::HalfOpen;
                    inner.half_open_probe = false;
                    inner.stats.breaker_trips = inner.stats.breaker_trips.saturating_add(1);
                    inner.stats.breaker_state = BreakerState::HalfOpen;
                }
                inner.window_outcomes = 0;
                inner.window_failures = 0;
            }
        }
        if failed {
            inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        } else {
            inner.consecutive_failures = 0;
        }
    }

    /// Sliding-window position for dashboards: `(outcomes, failures)` in
    /// the current open window. Mirrored into `SnubberStats` on settle.
    /// Planned failure path: rollback NOW, no breaker trip.
    pub fn abort(mut self, reason: &'static str) -> SnubberEvent<()> {
        let guard = self.guard.take().expect("guard already settled");
        let event = guard.abort(reason);
        self.settled = true;
        let node = self.node.clone();
        let is_probe = self.is_probe;
        let lane = self.priority.code().min(2) as usize;
        core::mem::forget(self);
        let waker = {
            let mut inner = node.lock().unwrap_or_else(|p| {
                let mut g = p.into_inner();
                g.stats.poison_recoveries = g.stats.poison_recoveries.saturating_add(1);
                hard_clamp_slots(&mut g);
                g
            });
            release_slot_inner(&mut inner, lane);
            inner.stats.explicit_aborts = inner.stats.explicit_aborts.saturating_add(1);
            // Planned aborts do NOT feed the breaker streak — but a
            // failed probe still re-opens: a probe that aborts proved
            // the downstream is still sick.
            if is_probe {
                inner.breaker = BreakerState::Open;
                inner.half_open_probe = false;
                inner.stats.breaker_state = BreakerState::Open;
            }
            inner.waiters.pop_front()
        };
        if let Some(w) = waker {
            w.wake();
        }
        event
    }

    /// Settle an implicit drop: count the bug, feed the breaker, report.
    ///
    /// Bullet 3 — slot leak, CLOSED BY CONSTRUCTION (not by audit):
    /// every settle path (`commit` / `abort` / `check_timeout` timeout /
    /// THIS function) decrements `in_flight` + `queued` exactly once,
    /// guarded by `self.settled`. The slot lifetime == the guard lifetime:
    /// success, `?`, panic, timeout — the counter always comes home.
    /// `EvictOldest` admission deliberately overbooks by one (the evicted
    /// owner's drop settles as its own implicit-drop — honest accounting).
    fn settle_implicit(&mut self) {
        self.settled = true;
        let stage = self.stage;
        let lane = self.priority.code().min(2) as usize;
        let record = FaultRecord {
            kind: FaultKind::ImplicitDrop,
            stage,
            stage_label: self.stage_label,
            queue_wait_ms: self.queue_wait_ms,
            elapsed_ms: now_ms().saturating_sub(self.armed_at_ms),
            retry_count: self.retry_count,
            ..self.arming.clone()
        };
        let mut inner = self.node.lock().unwrap_or_else(|p| {
            let mut g = p.into_inner();
            g.stats.poison_recoveries = g.stats.poison_recoveries.saturating_add(1);
            hard_clamp_slots(&mut g);
            g
        });
        release_slot_inner(&mut inner, lane);
        inner.stats.implicit_drops = inner.stats.implicit_drops.saturating_add(1);
        Self::note_outcome(&mut inner, true);
        inner.stats.window_outcomes = inner.window_outcomes;
        inner.stats.window_failures = inner.window_failures;
        let trips = inner.trip_after_failures;
        if trips > 0 && inner.consecutive_failures >= trips {
            // Fresh trip: OPEN + half-open probe scheduled on next enter.
            if inner.breaker != BreakerState::Open {
                inner.breaker = BreakerState::Open;
                inner.stats.breaker_trips = inner.stats.breaker_trips.saturating_add(1);
                inner.stats.breaker_state = BreakerState::Open;
                // Schedule the recovery probe: next `enter` admits one
                // call in HalfOpen instead of failing fast forever.
                inner.breaker = BreakerState::HalfOpen;
                inner.half_open_probe = false;
                inner.stats.breaker_state = BreakerState::HalfOpen;
            } else {
                inner.breaker = BreakerState::HalfOpen;
                inner.half_open_probe = false;
                inner.stats.breaker_state = BreakerState::HalfOpen;
            }
        }
        // Probe that died implicitly: downstream still sick → back to Open.
        if self.is_probe {
            inner.breaker = BreakerState::Open;
            inner.half_open_probe = false;
            inner.stats.breaker_state = BreakerState::Open;
        }
        let mut outcome = SnubberNode::record_locked(&mut inner, FaultKind::ImplicitDrop, 0, 0);
        let waker = inner.waiters.pop_front();
        drop(inner);
        publish_fault_arc(&self.node, &mut outcome, &record);
        SnubberNode::emit(outcome, record);
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<F: FnOnce()> Drop for TrackedGuard<F> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // v0.1 guard runs the HARDWARE rollback first (Drop order:
        // fields drop after this body — but `guard` is an Option holding
        // the v0.1 guard; take + drop it explicitly for clarity).
        if let Some(g) = self.guard.take() {
            drop(g); // hardware rollback runs HERE
        }
        self.settle_implicit();
        let _ = self.node_name;
    }
}

/// One-shot check: is stderr attached to a real console?
///
/// Safe [`std::io::IsTerminal`] probe (stable since Rust 1.70, matching
/// our MSRV) — no FFI, no `unsafe`, `#![forbid(unsafe_code)]` stays.
/// Called once per `console_node` construction, never per fault.
fn console_attached() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}
