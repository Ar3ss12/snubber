//! Fault Policy Engine: per-kind triad COMMAND + LOG + METRIC.
//!
//! Core-only (`core` + `Copy` only, no `std`, no `alloc`): the catalog of
//! failures and the declarative policy types live here so even `no_std`
//! firmware can name its faults. The *execution* of policies (running a
//! custom command, printing, tagging metrics) lives in
//! [`crate::SnubberNode`], which owns the lock + hooks + console.
//!
//! Closure-capturing commands (`move || …`) live behind the `alloc`
//! feature (`crate::ClosurePolicy`, in `default` since v0.3): boxed,
//! executed outside the node lock like every hook.

#[cfg(feature = "alloc")]
extern crate alloc;

use core::fmt;

/// What went wrong, in one cheap `Copy` enum.
///
/// Core-only so firmware, dashboards and [`Backpressure`] can name faults
/// without pulling the `std` node. Sized as a dense catalog (see
/// [`FaultKind::COUNT`]) so the node keeps a per-kind policy table +
/// per-kind metric counters with zero hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultKind {
    /// Guard dropped without `commit`/`abort` — unchecked `?`, early
    /// return, dead channel. **This is the bug detector.**
    ImplicitDrop,
    /// A thread died inside the transaction (`panic!` / failed `unwrap`).
    /// Reported explicitly via `SnubberNode::report` from a
    /// `catch_unwind` boundary — the guard alone cannot tell *panic*
    /// from *plain early return*, so the supervisor names it.
    Panic,
    /// Task hung past its heartbeat deadline (watchdog fires `report`).
    Timeout,
    /// Channel broke mid-action (dead WebView / pulled cable).
    /// Reported via `report`; the guard's implicit drop is the same
    /// incident seen from the hardware side.
    ChannelDisconnected,
    /// Bounded buffer filled (backpressure horizon hit).
    BufferOverflow,
    /// Packet arrived with `epoch <= last_seen` — UI lag / reorder.
    StaleEpoch,
    /// `max_in_flight` slots busy, `RejectNew` policy — node drowning.
    BulkheadFull,
    /// `queue_limit` hit, oldest event evicted to keep the freshest.
    EvictedOldest,
    /// Breaker OPEN: call rejected without touching the sick function.
    BreakerOpen,
    /// Input failed invariant validation (`cps == 0`, negative sleep…).
    /// Reported via `report` from the membrane BEFORE hardware is touched.
    PoisonedPayload,
    /// Supervised worker process terminated or hung past its lease
    /// (`abort()`, SIGKILL, `HangTimeout`). Reported by the supervisor
    /// via `report` — NEVER masked as `ChannelDisconnected` (v0.4:
    /// death is infrastructure catastrophe, not a flaky wire).
    ProcessDied,
    /// Live slot evicted by a higher-priority admission (Phase 1
    /// preemption): a `Priority::Critical` entry asked for a slot held
    /// only by `Priority::Background` work. The victim observes
    /// `Directive::Preempted` on its `TaskConduit` and settles itself
    /// (abort/drop); the newcomer is counted here. Hydraulic shedding,
    /// never nuclear — the node is healthy, the junior yielded.
    Preempted,
}

impl FaultKind {
    /// Number of variants — sizes the policy table and metric counters.
    pub const COUNT: usize = 12;

    /// Dense index for table lookup (matches declaration order).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::ImplicitDrop => 0,
            Self::Panic => 1,
            Self::Timeout => 2,
            Self::ChannelDisconnected => 3,
            Self::BufferOverflow => 4,
            Self::StaleEpoch => 5,
            Self::BulkheadFull => 6,
            Self::EvictedOldest => 7,
            Self::BreakerOpen => 8,
            Self::PoisonedPayload => 9,
            Self::ProcessDied => 10,
            Self::Preempted => 11,
        }
    }

    /// All variants in index order (for dashboard rendering).
    pub const ALL: [Self; Self::COUNT] = [
        Self::ImplicitDrop,
        Self::Panic,
        Self::Timeout,
        Self::ChannelDisconnected,
        Self::BufferOverflow,
        Self::StaleEpoch,
        Self::BulkheadFull,
        Self::EvictedOldest,
        Self::BreakerOpen,
        Self::PoisonedPayload,
        Self::ProcessDied,
        Self::Preempted,
    ];

    /// Nuclear (hull-breach) vs hydraulic (pressure-shedding) contour.
    ///
    /// * `true` — catastrophic: the task died anomalously
    ///   (`Panic`, `Timeout`, `ProcessDied`) or the transaction was
    ///   abandoned without settle (`ImplicitDrop` — silent panic / `?` /
    ///   forgotten `commit`). Feeds the breaker streak, may isolate.
    /// * `false` — hydraulic shedding / entry hygiene: the node is healthy
    ///   and holding the perimeter (`BulkheadFull`, `BufferOverflow`,
    ///   `EvictedOldest`, `StaleEpoch`, `BreakerOpen`, `RateLimited`
    ///   mapped onto bulkhead counters), or the input/wire was at fault
    ///   (`ChannelDisconnected` graceful drain, `PoisonedPayload` membrane
    ///   cut, queue TTL / payload budget). Counted, never trips the streak.
    ///
    /// Single source of truth for [`crate::SnubberNode::report`]: the old
    /// hardcoded `feeds_streak` match is this method. `const`, core-only.
    #[must_use]
    pub const fn is_catastrophic(self) -> bool {
        match self {
            Self::ImplicitDrop | Self::Panic | Self::Timeout | Self::ProcessDied => true,
            Self::ChannelDisconnected
            | Self::BufferOverflow
            | Self::StaleEpoch
            | Self::BulkheadFull
            | Self::EvictedOldest
            | Self::BreakerOpen
            | Self::PoisonedPayload
            | Self::Preempted => false,
        }
    }
}

/// Admission priority lane (Phase 1 preemption).
///
/// Core-only `Copy` data — no logic lives here. The node interprets it:
/// * `Background(0)` — trash lane: logs, presets, slow UI repaints.
///   Yields to `Critical` on demand (cooperative `Preempted`).
/// * `Normal(1)` — ordinary commands. Never preempts, never preempted
///   by anyone except `Critical` (and only when the whole node holds
///   nothing but `Background` — strict condition, no inversion).
/// * `Critical(2)` — emergency stop, panic recovery, operator kill.
///   When the bulkhead is full of pure `Background`, requests one
///   cooperative yield instead of bouncing as `BulkheadFull`.
///
/// Ordering is the discriminant: higher outranks lower. Default is
/// `Normal` — every existing `enter*` call keeps its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Priority {
    /// Slow background work, first to yield.
    Background = 0,
    /// Ordinary commands (default).
    #[default]
    Normal = 1,
    /// Emergency lane: stop / recovery / operator override.
    Critical = 2,
}

impl Priority {
    /// Discriminant as a `u8` code (for lock-held handoffs — `Copy` only).
    #[inline]
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Background => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }

    /// Rebuild from a code (`>= 2` saturates to `Critical` — never panics).
    #[inline]
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Background,
            1 => Self::Normal,
            _ => Self::Critical,
        }
    }

    /// Strict preemption rule: ONLY `Critical` over pure `Background`
    /// may ask for a yield. `Critical` vs `Normal` does NOT preempt
    /// (no inversion), `Normal` never preempts anyone.
    #[inline]
    #[must_use]
    pub const fn may_preempt(self) -> bool {
        matches!(self, Self::Critical)
    }
}

/// Static-or-owned fault detail: `no_std` firmware passes `&'static str`
/// with zero cost; `alloc` binaries format live values
/// (`interval=359999640000`) without losing the static fast path.
///
/// `FaultDetail::detail()` borrows the text either way — dashboards and
/// hooks never care which variant they hold.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FaultDetail {
    /// Zero-cost static reason (`"cps == 0"`).
    Static(&'static str),
    /// Owned dynamic reason (`format!("interval={v}")`).
    #[cfg(feature = "alloc")]
    Owned(alloc::string::String),
}

impl FaultDetail {
    /// Borrow the text behind either variant.
    #[inline]
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Static(s) => s,
            #[cfg(feature = "alloc")]
            Self::Owned(s) => s.as_str(),
        }
    }

    /// `true` for the zero-cost static variant.
    #[inline]
    #[must_use]
    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static(_))
    }
}

impl From<&'static str> for FaultDetail {
    #[inline]
    fn from(s: &'static str) -> Self {
        Self::Static(s)
    }
}

#[cfg(feature = "alloc")]
impl From<alloc::string::String> for FaultDetail {
    #[inline]
    fn from(s: alloc::string::String) -> Self {
        Self::Owned(s)
    }
}

#[cfg(feature = "alloc")]
impl From<&alloc::string::String> for FaultDetail {
    #[inline]
    fn from(s: &alloc::string::String) -> Self {
        Self::Owned(s.clone())
    }
}

impl fmt::Display for FaultDetail {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail())
    }
}

// COMPILE-TIME GATES (zero runtime, zero deps, MSRV 1.70-safe):
// `panic!` in const context is stable since 1.57, `size_of` const since
// 1.24 — this block is a build-time guillotine, not a test. Break the
// catalog and the build dies with the CRITICAL line, not a silent drift.
const _: () = {
    assert!(
        FaultKind::COUNT == 12,
        "CRITICAL: FaultKind::COUNT drifted — update policy tables + SnapshotV1::reported"
    );
    assert!(
        FaultKind::COUNT == FaultKind::ALL.len(),
        "CRITICAL: FaultKind::ALL out of sync with COUNT"
    );
    assert!(
        FaultKind::Preempted.index() + 1 == FaultKind::COUNT,
        "CRITICAL: Preempted must stay the last dense index"
    );
    assert!(
        Priority::Background as u8 == 0,
        "CRITICAL: Background lane code drifted from 0"
    );
    assert!(
        Priority::Normal as u8 == 1,
        "CRITICAL: Normal lane code drifted from 1"
    );
    assert!(
        Priority::Critical as u8 == 2,
        "CRITICAL: Critical lane code drifted from 2"
    );
};

/// Metric sample for one fault firing: static tag + static node name.
/// Exporters (Prometheus/OpenMetrics/ETW) poll [`crate::SnubberNode::stats`]
/// and [`crate::SnubberNode::reported`]; this struct is the single-row
/// unit they iterate: `(tag, node, kind_index, count)`.
/// `kind: usize` is deliberate: samples NEVER go on the wire as bytes
/// (in-memory exporter rows only). Wire structs are `u32/u64/u8` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MetricSample {
    /// Static metric tag from the policy (`"stale_packets_dropped"`).
    pub tag: &'static str,
    /// Node that fired.
    pub node: &'static str,
    /// Dense kind index (`FaultKind::index`, node module).
    pub kind: usize,
    /// How many times this (tag, kind) fired on this node.
    pub count: u64,
}

/// Severity of a fault report. The policy decides the noise level —
/// stale UI packets stay `Debug`, a dead driver goes `Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum LogLevel {
    /// Routine telemetry (stale packets, evictions).
    Debug,
    /// Something happened, worth a glance.
    #[default]
    Info,
    /// Backpressure applied, operator should notice.
    Warn,
    /// Hardware rollback fired, immediate attention.
    Error,
    /// Thread crash / breaker trip, wake somebody up.
    Critical,
}

/// Command the node executes when a fault of a given kind fires.
///
/// Evaluated OUTSIDE the node lock (same guarantee as hooks).
///
/// Honest matrix (0.3 audit — spec vs code):
/// * `DiscardSilently / BreakerOpen / RollbackAndAlert / TripBreaker /
///   Custom(fn())` — core-only, `Copy`, heap-free. `Custom` is a plain
///   `fn` precisely so policies stay `const`-constructible firmware data.
/// * `BlockCaller(ms)` — core-only DECLARATION, `std` execution: parks the
///   CALLER thread up to `ms` milliseconds (backpressure you can feel),
///   then admits-or-rejects. Implemented in `enter_inner`, not in hooks.
/// * Capturing `move ||` commands — NOT here (a `Box` cannot be `Copy`).
///   They live behind `alloc` as [`ClosurePolicy`](crate::ClosurePolicy)
///   and run through the same outside-the-lock path.
#[derive(Clone, Copy)]
pub enum FaultAction {
    /// Do nothing to the system (counters + hooks still fire).
    /// Ideal for stale UI packets.
    DiscardSilently,
    /// Fail fast as if the breaker were open for this one call.
    BreakerOpen,
    /// Run the hardware rollback NOW and set the UI-alert flag.
    /// (For entry-time faults no guard is armed yet — the alert flag is
    /// set immediately and the rollback runs at the next implicit drop.)
    RollbackAndAlert,
    /// Trip the breaker OPEN immediately (isolate the sick module),
    /// regardless of the consecutive-failure streak.
    TripBreaker,
    /// Developer self-healing: restart driver, resubscribe Win32 hooks, …
    /// Must be fast, non-panicking, lock-free — same contract as any
    /// hot-path destructor. Plain `fn` keeps the policy `Copy` and the
    /// node `alloc`-minimal. Need captures? Use `ClosurePolicy` (`alloc`).
    Custom(fn()),
    /// Backpressure with teeth: park the CALLER up to `ms` milliseconds
    /// waiting for a bulkhead slot, then admit — or reject if the slot
    /// never frees. `0` = try once, never sleep. Only meaningful on
    /// `BulkheadFull` / `BufferOverflow` kinds; elsewhere behaves as
    /// `DiscardSilently`. `std`-only execution (uses thread parking).
    ///
    /// ASYNC-FORBIDDEN: parks the OS thread via `std::thread::sleep`.
    /// Under Tokio this freezes the whole worker thread — in async code
    /// use `AdviseOnly` + `tokio::time::sleep(err.retry_after_ms()).await`.
    BlockCaller(u64),
}

impl fmt::Debug for FaultAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DiscardSilently => write!(f, "DiscardSilently"),
            Self::BreakerOpen => write!(f, "BreakerOpen"),
            Self::RollbackAndAlert => write!(f, "RollbackAndAlert"),
            Self::TripBreaker => write!(f, "TripBreaker"),
            Self::Custom(_) => write!(f, "Custom(..)"),
            Self::BlockCaller(ms) => write!(f, "BlockCaller({ms}ms)"),
        }
    }
}

/// Per-fault-kind triad: COMMAND + LOG + METRIC.
///
/// Declarative: `FaultPolicy::new(action).log(level, msg).metric(tag)`.
/// `const`-constructible, so default tables are compile-time data.
#[derive(Debug, Clone, Copy)]
pub struct FaultPolicy {
    /// What to physically do.
    pub action: FaultAction,
    /// Report severity (`None` = fully silent, counters still move).
    pub log_level: Option<LogLevel>,
    /// Static log line attached to the report.
    pub log_msg: Option<&'static str>,
    /// Metric tag incremented on the dashboard.
    pub metric_tag: Option<&'static str>,
}

impl FaultPolicy {
    /// Start a policy with a command; log defaults to `Info`, no metric.
    #[must_use]
    pub const fn new(action: FaultAction) -> Self {
        Self {
            action,
            log_level: Some(LogLevel::Info),
            log_msg: None,
            metric_tag: None,
        }
    }

    /// Set severity + static message.
    #[must_use]
    pub const fn log(mut self, level: LogLevel, msg: &'static str) -> Self {
        self.log_level = Some(level);
        self.log_msg = Some(msg);
        self
    }

    /// Silence all reporting for this kind (counters still move).
    #[must_use]
    pub const fn silent(mut self) -> Self {
        self.log_level = None;
        self.log_msg = None;
        self
    }

    /// Attach a metric tag incremented on every fault of this kind.
    #[must_use]
    pub const fn metric(mut self, tag: &'static str) -> Self {
        self.metric_tag = Some(tag);
        self
    }

    /// Shorthand: command + level + message + metric in one `const` expr.
    #[must_use]
    pub const fn arm(
        action: FaultAction,
        level: LogLevel,
        msg: &'static str,
        metric: &'static str,
    ) -> Self {
        Self {
            action,
            log_level: Some(level),
            log_msg: Some(msg),
            metric_tag: Some(metric),
        }
    }
}

impl Default for FaultPolicy {
    fn default() -> Self {
        Self::new(FaultAction::DiscardSilently)
    }
}

/// Dry instrument readings of wall resistance (Newton's 3rd law, info layer).
///
/// Returned BY the wall, never imposing: the caller decides whether to
/// sleep, shed, requeue or retry. `Copy`, `core`-only — realtime-safe to
/// read even where sleeping is forbidden (audio, games).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Backpressure {
    /// Advised wait before the next attempt, ms.
    pub retry_after_ms: u64,
    /// How deep the node is under water: 0 (free) ..= 255 (wall).
    pub depth: u8,
    /// Which fault kind presses (BulkheadFull / RateLimited / BreakerOpen).
    pub kind: super::FaultKind,
}

impl Backpressure {
    /// Build from parts (used by the node + custom friction authors).
    #[inline]
    #[must_use]
    pub const fn new(retry_after_ms: u64, depth: u8, kind: super::FaultKind) -> Self {
        Self {
            retry_after_ms,
            depth,
            kind,
        }
    }
}

/// Custom friction math: `(in_flight, max_in_flight) -> wait_ms`.
///
/// Plain `fn` so it stays `Copy` and `const`-friendly. Default is the soft
/// linear spring (`1 + depth * max_wait / 255`); hard systems plug a
/// quadratic/exponential curve or a fixed hardware cycle.
pub type FrictionFn = fn(u32, u32) -> u64;

/// Custom pressure reaction: what to do when the wall pushes back.
///
/// Runs OUTSIDE the node lock (same guarantee as hooks): may log, export
/// metrics, reprioritize — must not call back into `enter` synchronously
/// on realtime paths (read `depth`, decide, return).
pub type PressureFn = fn(Backpressure);

/// Who owns the wait when the wall pushes back (node-level policy).
///
/// Orthogonal to per-kind `FaultAction`: the action says WHAT happens on a
/// firing, this says WHO SLEEPS when admission is denied. Default is
/// `AdviseOnly` — the node never touches the caller thread.
///
/// NOTE: no `PartialEq` by design — function pointers have no meaningful
/// identity (`Custom(f) == Custom(f)` would lie). Compare discriminants
/// via [`BackpressurePolicy::kind_name`] in tests.
#[derive(Clone, Copy, Debug)]
pub enum BackpressurePolicy {
    /// Passive (default): never park the caller. The `Err` carries the
    /// readings, the caller paces itself. For audio, games, realtime.
    AdviseOnly,
    /// Park the caller up to `max_wait_ms` on the friction-computed wait,
    /// then admit-or-reject once. For daemons, workers, scripts.
    /// `shed_deeper_than`: when `Some(d)`, skip the sleep and reject
    /// immediately once `depth > d` (load shedding fused into the block).
    AdaptiveBlock {
        /// Hard cap on one park, ms.
        max_wait_ms: u64,
        /// Reject immediately past this depth (`None` = always park).
        shed_deeper_than: Option<u8>,
    },
    /// Call `PressureFn` with the readings (outside the lock), then reject.
    /// For telemetry reporters, priority droppers, custom shedders.
    Custom(PressureFn),
}

impl Default for BackpressurePolicy {
    #[inline]
    fn default() -> Self {
        Self::AdviseOnly
    }
}

impl BackpressurePolicy {
    /// Discriminant name for tests/dashboards (no `PartialEq` on fn ptrs).
    #[inline]
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::AdviseOnly => "AdviseOnly",
            Self::AdaptiveBlock { .. } => "AdaptiveBlock",
            Self::Custom(_) => "Custom",
        }
    }
}
