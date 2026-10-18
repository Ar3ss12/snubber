//! Fault Policy Engine: per-kind triad COMMAND + LOG + METRIC.
//!
//! Core-only (`core` + `Copy` only, no `std`, no `alloc`): the catalog of
//! failures and the declarative policy types live here so even `no_std`
//! firmware can classify faults. Policy execution (running custom actions,
//! emitting telemetry, incrementing metrics) is handled by [`crate::SnubberNode`].
//!
//! Capturing commands (`move || …`) require heap allocation and reside in
//! [`crate::ClosurePolicy`] under the `alloc` feature.

#[cfg(feature = "alloc")]
extern crate alloc;

use core::fmt;

/// Categorized failure classification for transactional and runtime faults.
///
/// Dense, zero-allocation `Copy` enum sized by [`FaultKind::COUNT`].
/// Enables indexed table lookups and metrics tracking without hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultKind {
    /// Guard dropped without explicit `commit`/`abort` (early return, `?`, unhandled drop).
    ImplicitDrop,
    /// Thread panic intercepted within a protected transaction boundary.
    Panic,
    /// Execution exceeded configured lease or heartbeat deadline.
    Timeout,
    /// Underlying channel disconnected during operation.
    ChannelDisconnected,
    /// Bounded queue or buffer capacity exhausted under backpressure.
    BufferOverflow,
    /// Packet arrived with a stale sequence or generation (`epoch <= last_seen`).
    StaleEpoch,
    /// Bulkhead saturated under `RejectNew` policy; admission denied.
    BulkheadFull,
    /// Waiter queue capacity exceeded; oldest item evicted under `EvictOldest`.
    EvictedOldest,
    /// Circuit breaker is in OPEN state; call rejected without invoking target.
    BreakerOpen,
    /// Input failed invariant boundary checks prior to resource engagement.
    PoisonedPayload,
    /// Supervised child process terminated anomalously or timed out past lease.
    ProcessDied,
    /// Work slot relinquished to satisfy a higher-priority `Priority::Critical` admission.
    /// Classified as operational load shedding; does not indicate system failure.
    Preempted,
}

impl FaultKind {
    /// Total number of variants — matches lookup table dimensions.
    pub const COUNT: usize = 12;

    /// Dense array index corresponding to declaration order.
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

    /// All variants arranged in index order.
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

    /// Discriminates unrecoverable failures from operational backpressure shedding.
    ///
    /// * `true` — Catastrophic: anomalous termination (`Panic`, `Timeout`, `ProcessDied`)
    ///   or uncommitted drops (`ImplicitDrop`). Feeds circuit breaker consecutive failure streaks.
    /// * `false` — Operational Shedding: defensive backpressure limits (`BulkheadFull`,
    ///   `BufferOverflow`, `EvictedOldest`, `StaleEpoch`, `BreakerOpen`, `PoisonedPayload`, `Preempted`).
    ///   Tracked in telemetry; does not increment breaker failure streaks.
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

/// Admission priority classification for concurrency arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Priority {
    /// Non-latency-critical background operations; first to yield under load.
    Background = 0,
    /// Default lane for standard application traffic.
    #[default]
    Normal = 1,
    /// Critical lane: emergency procedures, resets, and high-priority commands.
    Critical = 2,
}

impl Priority {
    /// Numerical representation of priority ranking.
    #[inline]
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Background => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }

    /// Decodes a numerical rank, saturating unknown values to `Critical`.
    #[inline]
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Background,
            1 => Self::Normal,
            _ => Self::Critical,
        }
    }

    /// Determines if this priority tier is authorized to request preemption.
    #[inline]
    #[must_use]
    pub const fn may_preempt(self) -> bool {
        matches!(self, Self::Critical)
    }
}

/// Dynamic or static error context accompanying fault events.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FaultDetail {
    /// Zero-allocation static descriptor.
    Static(&'static str),
    /// Formatted heap string for dynamic diagnostics.
    #[cfg(feature = "alloc")]
    Owned(alloc::string::String),
}

impl FaultDetail {
    /// Borrows the underlying diagnostic string.
    #[inline]
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Static(s) => s,
            #[cfg(feature = "alloc")]
            Self::Owned(s) => s.as_str(),
        }
    }

    /// Returns `true` if backed by a static reference.
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

// COMPILE-TIME INVARIANTS:
// Enforces table bounds and alignment guarantees at build time.
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

/// Telemetry metric sample capturing fault occurrences per node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MetricSample {
    /// Metric identifier tag.
    pub tag: &'static str,
    /// Originating node label.
    pub node: &'static str,
    /// Index corresponding to [`FaultKind::index`].
    pub kind: usize,
    /// Aggregated incident count.
    pub count: u64,
}

/// Operational severity levels for diagnostic reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum LogLevel {
    /// Telemetry and low-priority operational notices.
    Debug,
    /// Routine lifecycle events.
    #[default]
    Info,
    /// Non-fatal backpressure warnings.
    Warn,
    /// Actionable failures triggering rollbacks.
    Error,
    /// Breaker trips and thread termination events.
    Critical,
}

/// Action executed by the node upon encountering a classified fault.
///
/// NOTE: Intentionally does not implement `PartialEq` or `Eq` due to `Custom(fn())`.
/// Function pointer comparisons under Identical Code Folding (ICF) and across codegen
/// units do not guarantee stable identity. Use [`FaultAction::kind_name`] or `matches!`.
#[derive(Clone, Copy)]
pub enum FaultAction {
    /// Suppress action; increment internal telemetry counters only.
    DiscardSilently,
    /// Immediately reject caller as if the circuit breaker were OPEN.
    BreakerOpen,
    /// Execute rollback routine immediately and signal diagnostic alert.
    RollbackAndAlert,
    /// Force circuit breaker into OPEN state immediately.
    TripBreaker,
    /// Invokes a custom function pointer outside the node lock.
    Custom(fn()),
    /// Synchronously parks caller thread up to `ms` waiting for capacity.
    ///
    /// # Safety & Runtime Warnings
    /// Invokes `std::thread::sleep`. Forbidden in async contexts under Tokio
    /// to avoid worker pool starvation. Use async gate pacing instead.
    BlockCaller(u64),
}

impl FaultAction {
    /// Returns the variant name for testing and diagnostics.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::DiscardSilently => "DiscardSilently",
            Self::BreakerOpen => "BreakerOpen",
            Self::RollbackAndAlert => "RollbackAndAlert",
            Self::TripBreaker => "TripBreaker",
            Self::Custom(_) => "Custom",
            Self::BlockCaller(_) => "BlockCaller",
        }
    }
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

/// Configured policy triad mapping fault kinds to actions, logs, and metrics.
#[derive(Debug, Clone, Copy)]
pub struct FaultPolicy {
    /// Action to execute.
    pub action: FaultAction,
    /// Logging severity threshold.
    pub log_level: Option<LogLevel>,
    /// Optional static log message.
    pub log_msg: Option<&'static str>,
    /// Dashboard metric label.
    pub metric_tag: Option<&'static str>,
}

impl FaultPolicy {
    /// Initializes a policy with a target action; defaults to `LogLevel::Info`.
    #[must_use]
    pub const fn new(action: FaultAction) -> Self {
        Self {
            action,
            log_level: Some(LogLevel::Info),
            log_msg: None,
            metric_tag: None,
        }
    }

    /// Configures logging severity and associated message.
    #[must_use]
    pub const fn log(mut self, level: LogLevel, msg: &'static str) -> Self {
        self.log_level = Some(level);
        self.log_msg = Some(msg);
        self
    }

    /// Disables console output for this fault (metrics remain active).
    #[must_use]
    pub const fn silent(mut self) -> Self {
        self.log_level = None;
        self.log_msg = None;
        self
    }

    /// Configures an associated telemetry metric tag.
    #[must_use]
    pub const fn metric(mut self, tag: &'static str) -> Self {
        self.metric_tag = Some(tag);
        self
    }

    /// Shorthand constructor configuring action, log level, message, and metric.
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

/// Backpressure telemetry readings returned during admission rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Backpressure {
    /// Advised wait period before subsequent retry, in milliseconds.
    pub retry_after_ms: u64,
    /// Concurrency saturation depth: 0 (unloaded) to 255 (fully saturated).
    pub depth: u8,
    /// Triggering fault condition.
    pub kind: FaultKind,
}

impl Backpressure {
    /// Constructs a telemetry record.
    #[inline]
    #[must_use]
    pub const fn new(retry_after_ms: u64, depth: u8, kind: FaultKind) -> Self {
        Self {
            retry_after_ms,
            depth,
            kind,
        }
    }
}

/// Custom friction calculation: `(in_flight, max_in_flight) -> wait_ms`.
pub type FrictionFn = fn(u32, u32) -> u64;

/// Callback invoked outside node locks during backpressure shedding.
pub type PressureFn = fn(Backpressure);

/// Node-level policy defining caller pacing under saturation.
#[derive(Clone, Copy, Debug)]
pub enum BackpressurePolicy {
    /// Non-blocking mode: returns backpressure readings immediately to caller.
    AdviseOnly,
    /// Synchronously blocks caller up to `max_wait_ms` under saturation.
    /// Rejects immediately without waiting if `depth` exceeds `shed_deeper_than`.
    AdaptiveBlock {
        /// Maximum duration to block caller thread, in milliseconds.
        max_wait_ms: u64,
        /// Saturation depth cutoff for immediate rejection.
        shed_deeper_than: Option<u8>,
    },
    /// Executes custom handler with telemetry data, then rejects caller.
    Custom(PressureFn),
}

impl Default for BackpressurePolicy {
    #[inline]
    fn default() -> Self {
        Self::AdviseOnly
    }
}

impl BackpressurePolicy {
    /// Textual identifier for diagnostic export.
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
