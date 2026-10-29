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
#[non_exhaustive]
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
    /// Token bucket exhausted (`rate_per_sec`); admission shed before a slot
    /// was taken. Operational load shedding with its OWN column — never
    /// merged into `BulkheadFull`, never fed to that kind's policy.
    RateLimited,
    /// A rollback body ran past the armed `rollback_budget_ms`. The body
    /// RETURNED (the transaction already settled); this record names the
    /// overrun for the post-mortem. Never a timeout, never a failure
    /// streak: a slow compensation is an observation, not a dead task.
    RollbackStall,
}

impl FaultKind {
    /// Total number of variants — matches lookup table dimensions.
    pub const COUNT: usize = 14;

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
            Self::RateLimited => 12,
            Self::RollbackStall => 13,
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
        Self::RateLimited,
        Self::RollbackStall,
    ];

    /// Discriminates unrecoverable failures from operational backpressure shedding.
    ///
    /// * `true` — Catastrophic: anomalous termination (`Panic`, `Timeout`, `ProcessDied`)
    ///   or uncommitted drops (`ImplicitDrop`). Feeds circuit breaker consecutive failure streaks.
    /// * `false` — Operational Shedding: defensive backpressure limits (`BulkheadFull`,
    ///   `BufferOverflow`, `EvictedOldest`, `StaleEpoch`, `BreakerOpen`, `PoisonedPayload`,
    ///   `Preempted`, `RateLimited`) and observations (`RollbackStall` — the body returned).
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
            | Self::Preempted
            | Self::RateLimited
            | Self::RollbackStall => false,
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
    /// Number of priority lanes — FORMAT CONTRACT, not a tunable.
    ///
    /// The wire histogram, telemetry arrays and preemption law are
    /// indexed `0..COUNT`; changing it is a snapshot/wire version bump.
    pub const COUNT: usize = 3;

    /// Every lane in ascending order (`Background → Normal → Critical`).
    pub const ALL: [Priority; Self::COUNT] = [Self::Background, Self::Normal, Self::Critical];

    /// Highest valid lane index, for bounds-clamping per-lane counters.
    pub(crate) const MAX_INDEX: usize = Self::COUNT - 1;

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

// The doc table calls `Priority::COUNT` a compile-asserted format ceiling;
// this is that assert. Changing COUNT is a snapshot/wire version bump.
const _: () = assert!(
    Priority::COUNT == 3,
    "Priority::COUNT is a published wire contract (3 priority lanes)"
);

/// Victim set for a cooperative preempt request.
///
/// [`Priority::may_preempt`] names the aggressor under the default
/// policy; a policy only changes WHO counts as a victim:
///
/// * [`Self::BackgroundOnly`] (default) — knock only when every held slot
///   is `Background`; `Critical` vs `Normal` never preempts.
/// * [`Self::StrictSenior`] — knock whenever every held slot is strictly
///   junior to the aggressor: `Normal` may knock `Background`, `Critical`
///   may knock `Background` + `Normal`. Equal-or-senior occupancy never
///   preempts (no inversion).
///
/// The node never forces: victims observe `Directive::Preempted` on their
/// own conduit and settle themselves (R5 — zero deadlock).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreemptPolicy {
    /// `Critical` over pure-`Background` occupancy only.
    #[default]
    BackgroundOnly,
    /// Knock any strictly junior occupancy, from `Normal` upward.
    StrictSenior,
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
        FaultKind::COUNT == 14,
        "CRITICAL: FaultKind::COUNT drifted — update policy tables + SnapshotV1::reported"
    );
    assert!(
        FaultKind::COUNT == FaultKind::ALL.len(),
        "CRITICAL: FaultKind::ALL out of sync with COUNT"
    );
    assert!(
        FaultKind::RollbackStall.index() + 1 == FaultKind::COUNT,
        "CRITICAL: RollbackStall must stay the last dense index"
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
    assert!(
        Priority::Background.code() == 0
            && Priority::Normal.code() == 1
            && Priority::Critical.code() == 2,
        "CRITICAL: Priority::code() must match enum discriminants"
    );
    assert!(
        Priority::MAX_INDEX + 1 == Priority::COUNT,
        "CRITICAL: Priority::MAX_INDEX must stay the highest dense lane index"
    );
    // Catastrophic classification contract:
    // Only uncommitted drops, panics, timeouts, and process deaths feed circuit breaker streaks.
    assert!(FaultKind::ImplicitDrop.is_catastrophic());
    assert!(FaultKind::Panic.is_catastrophic());
    assert!(FaultKind::Timeout.is_catastrophic());
    assert!(FaultKind::ProcessDied.is_catastrophic());
    assert!(!FaultKind::ChannelDisconnected.is_catastrophic());
    assert!(!FaultKind::BufferOverflow.is_catastrophic());
    assert!(!FaultKind::StaleEpoch.is_catastrophic());
    assert!(!FaultKind::BulkheadFull.is_catastrophic());
    assert!(!FaultKind::EvictedOldest.is_catastrophic());
    assert!(!FaultKind::BreakerOpen.is_catastrophic());
    assert!(!FaultKind::PoisonedPayload.is_catastrophic());
    assert!(!FaultKind::Preempted.is_catastrophic());
    assert!(!FaultKind::RateLimited.is_catastrophic());
    assert!(
        !FaultKind::RollbackStall.is_catastrophic(),
        "a slow rollback that RETURNED is an observation, not a failure streak"
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

    /// Same reading with `retry_after_ms` spread by [`jitter_ms`].
    ///
    /// Depth and kind are the diagnosis; only the advice moves. Pass a salt
    /// that differs per waiter (task id, attempt counter, thread index) or
    /// every waiter computes the same offset and the herd returns intact.
    #[inline]
    #[must_use]
    pub const fn jittered(&self, spread_percent: u8, salt: u64) -> Self {
        Self {
            retry_after_ms: jitter_ms(self.retry_after_ms, spread_percent, salt),
            depth: self.depth,
            kind: self.kind,
        }
    }
}

/// Deterministic offset for one salt: the SplitMix64 finalizer, not an RNG.
///
/// It mixes the caller's salt into every bit, so two salts one apart land far
/// apart. The crate owns no entropy source (`no_std`, no clock, no `getrandom`
/// without a dependency), so the caller supplies the variation and gets the
/// same answer for the same salt — which is what makes a jitter test possible.
#[inline]
const fn spread_hash(mut salt: u64) -> u64 {
    salt ^= salt >> 30;
    salt = salt.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    salt ^= salt >> 27;
    salt = salt.wrapping_mul(0x94d0_49bb_1331_11eb);
    salt ^ (salt >> 31)
}

/// `base_ms` moved by at most `spread_percent` in either direction.
///
/// The result lies in `[base − base·p/100, base + base·p/100]`, so the band
/// is symmetric around the advice the node gave and its mean stays `base_ms`.
/// `spread_percent == 0` (or `base_ms == 0`) returns the base unchanged;
/// `spread_percent == 100` is full jitter, `[0, 2·base]`.
///
/// This is spread, not randomness: the same salt always yields the same
/// millisecond count. `salt == 0` hashes to `0` and therefore always lands on
/// the low edge of the band — treat it as "no jitter" (`spread_percent = 0`)
/// rather than as a neutral value.
///
/// ```
/// use snubber::jitter_ms;
///
/// assert_eq!(jitter_ms(1_000, 0, 7), 1_000);          // no spread, no motion
/// assert_eq!(jitter_ms(1_000, 20, 7), jitter_ms(1_000, 20, 7));
/// let v = jitter_ms(1_000, 20, 7);
/// assert!((800..=1_200).contains(&v));                // ±20 % around the advice
/// ```
#[inline]
#[must_use]
pub const fn jitter_ms(base_ms: u64, spread_percent: u8, salt: u64) -> u64 {
    if base_ms == 0 || spread_percent == 0 {
        return base_ms;
    }
    let spread = ((base_ms as u128 * spread_percent as u128) / 100) as u64;
    if spread == 0 {
        return base_ms;
    }
    let span = spread.saturating_mul(2).saturating_add(1);
    let low = base_ms - spread;
    let offset = spread_hash(salt) % span;
    low.saturating_add(offset)
}

/// Additive-increase / multiplicative-decrease concurrency ceiling.
///
/// The classic congestion-control law, applied to admissions: each good
/// window buys one `step` of headroom, each pressure event keeps
/// `keep_percent` of the current ceiling. It converges on the largest limit
/// the downstream actually sustains instead of the one somebody guessed at
/// build time, and it never goes below `min` (livelock) or above `max`.
///
/// Feed it **one outcome per window** — a second of traffic, a batch, a
/// macro run — never one per admission: an additive step per admission makes
/// the ceiling grow without bound between two samples, which is not AIMD.
///
/// ```
/// use snubber::AimdLimit;
///
/// let mut limit = AimdLimit::new(4, 1, 16, 1, 50);
/// assert_eq!(limit.on_success(), 5);      // +1 per clean window
/// assert_eq!(limit.on_pressure(), 2);     // halve: 5 * 50 / 100
/// assert_eq!(limit.limit(), 2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AimdLimit {
    limit: u32,
    min: u32,
    max: u32,
    step: u32,
    keep_percent: u8,
}

impl AimdLimit {
    /// `keep_percent` is clamped to `1..=99` (a ceiling that never shrinks
    /// is not AIMD; one that shrinks to `0` is just `min` with extra steps),
    /// `max` to at least `1`, `step` to at least `1`, and `initial` into
    /// `min..=max`.
    #[must_use]
    pub const fn new(initial: u32, min: u32, max: u32, step: u32, keep_percent: u8) -> Self {
        let max = if max == 0 { 1 } else { max };
        let min = if min < max { min } else { max };
        let step = if step == 0 { 1 } else { step };
        let keep_percent = if keep_percent < 1 {
            1
        } else if keep_percent > 99 {
            99
        } else {
            keep_percent
        };
        let limit = if initial < min {
            min
        } else if initial > max {
            max
        } else {
            initial
        };
        Self {
            limit,
            min,
            max,
            step,
            keep_percent,
        }
    }

    /// Current ceiling.
    #[inline]
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }

    /// Floor the ceiling may never cross.
    #[inline]
    #[must_use]
    pub const fn min_limit(&self) -> u32 {
        self.min
    }

    /// Ceiling the ceiling may never cross.
    #[inline]
    #[must_use]
    pub const fn max_limit(&self) -> u32 {
        self.max
    }

    /// Additive step applied per clean window.
    #[inline]
    #[must_use]
    pub const fn step(&self) -> u32 {
        self.step
    }

    /// Fraction of the ceiling kept by a multiplicative decrease.
    #[inline]
    #[must_use]
    pub const fn keep_percent(&self) -> u8 {
        self.keep_percent
    }

    /// One clean window: add `step`, clamped to `max`.
    #[inline]
    pub fn on_success(&mut self) -> u32 {
        self.limit = self.limit.saturating_add(self.step).min(self.max);
        self.limit
    }

    /// One pressure window: keep `keep_percent` of the ceiling, never below
    /// `min`.
    #[inline]
    pub fn on_pressure(&mut self) -> u32 {
        let kept = (u64::from(self.limit) * u64::from(self.keep_percent) / 100) as u32;
        self.limit = if kept < self.min { self.min } else { kept };
        self.limit
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn additive_increase_stops_at_the_ceiling() {
        let mut limit = AimdLimit::new(4, 1, 6, 1, 50);
        assert_eq!(limit.on_success(), 5);
        assert_eq!(limit.on_success(), 6);
        assert_eq!(limit.on_success(), 6, "max is a wall, not a suggestion");
        assert_eq!(limit.limit(), 6);
        assert_eq!(limit.min_limit(), 1);
        assert_eq!(limit.max_limit(), 6);
        assert_eq!(limit.step(), 1);
        assert_eq!(limit.keep_percent(), 50);
    }

    #[test]
    fn multiplicative_decrease_keeps_the_configured_fraction() {
        let mut limit = AimdLimit::new(8, 1, 16, 1, 50);
        assert_eq!(limit.on_pressure(), 4);
        assert_eq!(limit.on_pressure(), 2);
        assert_eq!(limit.on_pressure(), 1);
        assert_eq!(
            limit.on_pressure(),
            1,
            "min is the floor: a ceiling of 0 would refuse everything forever"
        );
    }

    #[test]
    fn the_constructor_clamps_a_broken_policy() {
        let mut cramped = AimdLimit::new(0, 5, 2, 0, 0);
        assert_eq!(cramped.max_limit(), 2);
        assert_eq!(
            cramped.min_limit(),
            2,
            "a floor above the ceiling is the ceiling"
        );
        assert_eq!(cramped.limit(), 2);
        assert_eq!(
            cramped.step(),
            1,
            "a zero step is one, not a disabled increase"
        );
        assert_eq!(cramped.keep_percent(), 1);
        assert_eq!(cramped.on_pressure(), 2, "already at the floor");

        let wide = AimdLimit::new(100, 1, 8, 1, 250);
        assert_eq!(wide.limit(), 8);
        assert_eq!(wide.keep_percent(), 99, "a decrease that decreases");
    }

    #[test]
    fn jitter_is_deterministic_for_one_salt() {
        assert_eq!(jitter_ms(1_000, 20, 42), jitter_ms(1_000, 20, 42));
        assert!(
            (0..64).any(|salt| jitter_ms(1_000, 20, salt) != jitter_ms(1_000, 20, 42)),
            "distinct salts must land on distinct milliseconds"
        );
    }

    #[test]
    fn jitter_spreads_symmetrically_inside_the_band() {
        let mut below = 0u32;
        let mut above = 0u32;
        let mut seen = std::collections::HashSet::new();
        for salt in 0..512u64 {
            let v = jitter_ms(1_000, 20, salt);
            assert!(
                (800..=1_200).contains(&v),
                "salt {salt} produced {v} ms, outside the ±20% band"
            );
            seen.insert(v);
            if v < 1_000 {
                below += 1;
            } else if v > 1_000 {
                above += 1;
            }
        }
        assert!(
            below > 0 && above > 0,
            "the band is symmetric, not one-sided"
        );
        assert!(
            seen.len() > 100,
            "512 salts produced only {} distinct delays — a mixer that collapses is a herd",
            seen.len()
        );
    }

    #[test]
    fn jitter_is_off_without_a_spread() {
        assert_eq!(jitter_ms(1_000, 0, 7), 1_000);
        assert_eq!(jitter_ms(0, 50, 7), 0);
        assert_eq!(
            jitter_ms(3, 10, 7),
            3,
            "a spread that rounds to zero returns the base"
        );
    }

    #[test]
    fn jitter_never_wraps_near_the_top_of_the_range() {
        let spread = ((u64::MAX as u128 * 99) / 100) as u64;
        for salt in [0u64, 1, 9_999, u64::MAX] {
            let v = jitter_ms(u64::MAX, 99, salt);
            assert!(
                v >= u64::MAX - spread,
                "salt {salt} left the band: {v} below {}",
                u64::MAX - spread
            );
        }
    }

    #[test]
    fn a_jittered_reading_moves_only_the_delay() {
        let reading = Backpressure::new(1_000, 7, FaultKind::BulkheadFull);
        let moved = reading.jittered(20, 3);
        assert_eq!(moved.retry_after_ms, jitter_ms(1_000, 20, 3));
        assert_eq!(moved.depth, 7);
        assert_eq!(moved.kind, FaultKind::BulkheadFull);
    }
}
