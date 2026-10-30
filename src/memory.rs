//! Memory pressure accounting: pure `core`, no allocation, no measurement.
//!
//! The crate never reads RSS itself — `no_std` firmware, Linux `/proc`,
//! Win32 `GetProcessMemoryInfo` and a Tokio runtime all disagree on how.
//! The APP feeds one external sample into
//! [`SnubberNode::note_memory_kb`](crate::SnubberNode::note_memory_kb)
//! (cold path, once per second); every admission then observes a single
//! `AtomicBool`, never a sampler, never a lock held by foreign code.
//!
//! Limits are KiB. `0` disables a limit: soft-only nodes drain and let the
//! supervisor restart them, hard-only nodes refuse new admissions at the
//! hard limit and signal `MemoryVerdict::Hard` so the supervisor can
//! terminate without draining, both-zero nodes behave exactly as before
//! (zero cost).

/// Verdict for one external memory sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MemoryVerdict {
    /// `used < soft` (or no limits configured): admit normally.
    #[default]
    Ok,
    /// `used >= soft_kb`: refuse new admissions, let in-flight work settle.
    /// The supervisor may snapshot the drained node and restart it.
    Soft,
    /// `used >= hard_kb`: refuse new admissions and terminate now —
    /// no drain, the process is already over budget.
    Hard,
}

impl MemoryVerdict {
    /// `true` for [`Soft`](Self::Soft) and [`Hard`](Self::Hard).
    #[must_use]
    pub const fn is_pressured(self) -> bool {
        !matches!(self, Self::Ok)
    }

    /// `true` only for [`Hard`](Self::Hard).
    #[must_use]
    pub const fn is_hard(self) -> bool {
        matches!(self, Self::Hard)
    }

    /// Static text for diagnostics (`"ok"` / `"soft"` / `"hard"`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }
}

/// Soft/hard limits plus the last sample and its peak (KiB).
///
/// `Copy` integers only — PHASE A/C safe. `new` clamps `hard` up to
/// `soft` (a hard limit below the soft limit would fire earlier and
/// silently disable the drain phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MemoryWatch {
    soft_kb: u64,
    hard_kb: u64,
    used_kb: u64,
    peak_kb: u64,
}

impl MemoryWatch {
    /// Both limits disabled (zero cost, legacy behaviour).
    pub const OFF: Self = Self {
        soft_kb: 0,
        hard_kb: 0,
        used_kb: 0,
        peak_kb: 0,
    };

    /// `soft_kb` = drain threshold, `hard_kb` = terminate threshold.
    /// Either may be `0` (disabled); `hard < soft` is clamped to `soft`.
    #[must_use]
    pub const fn new(soft_kb: u64, hard_kb: u64) -> Self {
        let hard = if hard_kb != 0 && hard_kb < soft_kb {
            soft_kb
        } else {
            hard_kb
        };
        Self {
            soft_kb,
            hard_kb: hard,
            used_kb: 0,
            peak_kb: 0,
        }
    }

    /// `true` when at least one limit is armed.
    #[must_use]
    pub const fn is_configured(self) -> bool {
        self.soft_kb > 0 || self.hard_kb > 0
    }

    /// Configured soft limit, KiB (`0` = disabled).
    #[must_use]
    pub const fn soft_kb(self) -> u64 {
        self.soft_kb
    }

    /// Configured hard limit, KiB (`0` = disabled).
    #[must_use]
    pub const fn hard_kb(self) -> u64 {
        self.hard_kb
    }

    /// Last sampled value, KiB.
    #[must_use]
    pub const fn used_kb(self) -> u64 {
        self.used_kb
    }

    /// Worst sampled value since boot, KiB.
    #[must_use]
    pub const fn peak_kb(self) -> u64 {
        self.peak_kb
    }

    /// Verdict for the last sample. Pure integer compares.
    #[must_use]
    pub const fn verdict(self) -> MemoryVerdict {
        if self.hard_kb > 0 && self.used_kb >= self.hard_kb {
            MemoryVerdict::Hard
        } else if self.soft_kb > 0 && self.used_kb >= self.soft_kb {
            MemoryVerdict::Soft
        } else {
            MemoryVerdict::Ok
        }
    }

    /// Record one sample (KiB) and return its verdict.
    pub fn note(&mut self, used_kb: u64) -> MemoryVerdict {
        self.used_kb = used_kb;
        self.peak_kb = self.peak_kb.max(used_kb);
        self.verdict()
    }
}

// COMPILE-TIME GATE: the cold state stays 4 machine words — it lives in
// `NodeInner`, never on the admission stack.
const _: () = {
    assert!(
        core::mem::size_of::<MemoryWatch>() == 32,
        "CRITICAL: MemoryWatch must stay 32 bytes (4 x u64)"
    );
};

/// The trend ring behind [`MemoryTrend`] and `HandleWatch`: one fixed
/// window of `N` samples, oldest-overwritten, no clock and no allocation.
///
/// A leak is a DERIVATIVE, not a level. A verdict exists only when the ring
/// is full AND every sample is strictly greater than the one before it; the
/// growth is then `newest - oldest`. One sample that fails to beat its
/// predecessor clears the window, because a healthy process is allowed to
/// oscillate (a collection, a tray collapse, a pool draining).
///
/// Crate-internal on purpose: the public types that own a ring are
/// [`MemoryTrend`] and [`crate::HandleWatch`], so there is exactly one way
/// to ask "is this rising", not two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TrendRing<const N: usize> {
    ring: [u64; N],
    len: usize,
    next: usize,
}

impl<const N: usize> TrendRing<N> {
    /// Empty window. `N = 0` is a ring that can never report a leak.
    pub(crate) const fn new() -> Self {
        Self {
            ring: [0; N],
            len: 0,
            next: 0,
        }
    }

    /// Push one sample and return the growth of a strictly rising FULL
    /// window (`newest - oldest`), or `None` while the window is short or
    /// not monotonic.
    pub(crate) fn push(&mut self, sample: u64) -> Option<u64> {
        if N == 0 {
            return None;
        }
        self.ring[self.next] = sample;
        self.next = (self.next + 1) % N;
        if self.len < N {
            self.len += 1;
        }
        self.window().growth_kb()
    }

    /// The whole window in one read: how much evidence there is, whether it
    /// is full, whether it rose without a dip, and its ends. One pass, no
    /// allocation — the caller decides what the shape means.
    #[must_use]
    pub(crate) fn window(&self) -> TrendWindow {
        if N == 0 || self.len == 0 {
            return TrendWindow {
                samples: 0,
                full: false,
                rising_partial: false,
                rising: false,
                oldest: 0,
                newest: 0,
            };
        }
        // Not full yet: `next` is the first free slot, so sample 0 is oldest.
        // Full: `next` was just overwritten, which makes it the oldest.
        let oldest = if self.len < N {
            self.ring[0]
        } else {
            self.ring[self.next]
        };
        let newest = self.ring[(self.next + N - 1) % N];
        let mut rising_partial = self.len >= 2;
        if rising_partial {
            let mut previous = oldest;
            for i in 1..self.len {
                let sample = if self.len < N {
                    self.ring[i]
                } else {
                    self.ring[(self.next + i) % N]
                };
                if sample <= previous {
                    rising_partial = false;
                    break;
                }
                previous = sample;
            }
        }
        TrendWindow {
            samples: self.len,
            full: self.len == N,
            rising_partial,
            rising: self.len == N && rising_partial,
            oldest,
            newest,
        }
    }

    /// Growth per second over a full rising window, given the caller's tick.
    /// `None` while the window is short, not monotonic, or the interval or
    /// the window width cannot span time (`N < 2`, `sample_interval_ms == 0`).
    #[must_use]
    pub(crate) fn growth_rate_kb_sec(&self, sample_interval_ms: u64) -> Option<u64> {
        if N < 2 || sample_interval_ms == 0 {
            return None;
        }
        let growth = self.window().growth_kb()?;
        Some(rate_kb_sec(growth, N as u64 - 1, sample_interval_ms))
    }

    /// Classify the window against a trip mark. Pure read; the mark is a
    /// growth, so the same absolute level classifies differently on a short
    /// and a long window.
    #[must_use]
    pub(crate) fn classify(&self, trip_kb: u64, sample_interval_ms: u64) -> LeakClassification {
        let window = self.window();
        if let Some(growth) = window.growth_kb() {
            return if crosses_trip(growth, trip_kb) {
                LeakClassification::Rising {
                    growth_kb: growth,
                    rate_kb_sec: self.growth_rate_kb_sec(sample_interval_ms),
                }
            } else {
                LeakClassification::Stable { growth_kb: growth }
            };
        }
        if window.full {
            return LeakClassification::Sawtooth;
        }
        // A partial window never decides, but a long enough run of rising
        // samples that would clear the trip mark by the time the window
        // fills is worth naming early — advisory, never a latch.
        if let Some(growth) = window.partial_growth_kb() {
            let wide_enough = window.samples >= ADVISORY_MIN_SAMPLES && window.samples * 2 >= N;
            let projected = project_growth(growth, window.samples, N);
            if wide_enough && crosses_trip(projected, trip_kb) {
                return LeakClassification::Advisory {
                    growth_kb: growth,
                    projected_growth_kb: projected,
                    samples: window.samples,
                    rate_kb_sec: if sample_interval_ms == 0 {
                        None
                    } else {
                        Some(rate_kb_sec(
                            growth,
                            window.samples as u64 - 1,
                            sample_interval_ms,
                        ))
                    },
                };
            }
        }
        LeakClassification::Warming {
            samples: window.samples,
        }
    }

    /// Samples currently in the window (`N` once it has wrapped).
    #[must_use]
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Newest sample in the window, if any.
    #[must_use]
    pub(crate) const fn newest(&self) -> Option<u64> {
        if self.len == 0 {
            None
        } else {
            Some(self.ring[(self.next + N - 1) % N])
        }
    }
}

/// One read of a [`TrendRing`] window. Crate-internal: the public owner of
/// a ring is [`MemoryTrend`] / `crate::HandleWatch`, and it exposes the
/// answer, not the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct TrendWindow {
    /// Samples held (`N` once the ring has wrapped).
    pub(crate) samples: usize,
    /// `true` when the ring holds `N` samples: a verdict is possible.
    pub(crate) full: bool,
    /// `true` when every held sample beat the one before it — for a partial
    /// window too, which is what an advisory slope is read from.
    pub(crate) rising_partial: bool,
    /// `true` only for a full window whose every sample beat the one before
    /// it. Equality counts as a dip, like a fall does.
    pub(crate) rising: bool,
    /// Oldest sample in the window, KiB.
    pub(crate) oldest: u64,
    /// Newest sample in the window, KiB.
    pub(crate) newest: u64,
}

impl TrendWindow {
    /// `newest - oldest` when the window is full and rising; `None` otherwise.
    /// A partial window can look steep and still say nothing — one more
    /// sample may be the dip that ends it.
    #[must_use]
    pub(crate) const fn growth_kb(self) -> Option<u64> {
        if self.full && self.rising {
            Some(self.newest - self.oldest)
        } else {
            None
        }
    }

    /// `newest - oldest` over the held samples of a partial window that rose
    /// without a dip. `None` once the window is full (use [`growth_kb`]).
    ///
    /// [`growth_kb`]: Self::growth_kb
    #[must_use]
    pub(crate) const fn partial_growth_kb(self) -> Option<u64> {
        if !self.full && self.rising_partial && self.samples >= 2 {
            Some(self.newest - self.oldest)
        } else {
            None
        }
    }
}

/// Fewest samples an advisory slope may rest on. Below three a single noisy
/// tick is extrapolated across the whole window and reads as a trend.
pub(crate) const ADVISORY_MIN_SAMPLES: usize = 3;

/// Linear extrapolation of a partial window's growth to a full one:
/// `growth * (width - 1) / (samples - 1)`. Saturates instead of overflowing.
#[must_use]
pub(crate) const fn project_growth(growth_kb: u64, samples: usize, width: usize) -> u64 {
    if samples < 2 || width < 2 || samples > width {
        return growth_kb;
    }
    let projected = (growth_kb as u128) * (width as u128 - 1) / (samples as u128 - 1);
    if projected > u64::MAX as u128 {
        u64::MAX
    } else {
        projected as u64
    }
}

/// Does a monotonic window that grew this much name a leak? `trip_kb == 0`
/// disables the verdict.
const fn crosses_trip(growth_kb: u64, trip_kb: u64) -> bool {
    trip_kb > 0 && growth_kb >= trip_kb
}

/// What a full window looks like against a trip mark. [`Warming`] is the
/// honest answer while the evidence is short; the rest name a shape, not a
/// fault — the caller owns the reaction.
///
/// [`Warming`]: Self::Warming
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LeakClassification {
    /// The ring is not full yet: `samples` of `N` collected.
    Warming {
        /// How many samples the window holds so far.
        samples: usize,
    },
    /// Full, rising, but the growth is under the trip mark.
    Stable {
        /// `newest - oldest` over the window, KiB.
        growth_kb: u64,
    },
    /// Full, rising, and the growth crossed the trip mark.
    Rising {
        /// `newest - oldest` over the window, KiB.
        growth_kb: u64,
        /// KiB per second over the window; `None` when the caller armed no
        /// interval (`0`) or the window is a single sample wide (`N < 2`).
        rate_kb_sec: Option<u64>,
    },
    /// Full and NOT monotonic: something fell or flattened inside the window.
    /// A sawtooth leak reads here and never trips the monotonic verdict —
    /// this variant exists so that shape is nameable instead of invisible.
    Sawtooth,
    /// Partial window that has risen without a dip, for at least half the
    /// window and at least three samples, and whose slope
    /// projected across the full window would cross the trip mark.
    ///
    /// **Advisory, never a latch.** One more sample can still be the dip that
    /// ends the run, so this variant never answers `is_rising()`; a caller may
    /// log it, show it or refuse a *start* with its own policy, but a leak
    /// verdict stays the full window's job.
    Advisory {
        /// `newest - oldest` over the held samples, KiB.
        growth_kb: u64,
        /// That growth projected linearly across a full window, KiB.
        projected_growth_kb: u64,
        /// How many samples the advisory rests on (confidence, not a count of
        /// entries in the full window).
        samples: usize,
        /// KiB per second over the held samples; `None` when the caller armed
        /// no interval (`0`).
        rate_kb_sec: Option<u64>,
    },
}

impl LeakClassification {
    /// Static text for diagnostics (`"warming"` / `"stable"` / `"rising"` /
    /// `"sawtooth"` / `"advisory"`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Warming { .. } => "warming",
            Self::Stable { .. } => "stable",
            Self::Rising { .. } => "rising",
            Self::Sawtooth => "sawtooth",
            Self::Advisory { .. } => "advisory",
        }
    }

    /// `true` only for [`Rising`](Self::Rising). [`Advisory`](Self::Advisory)
    /// is a partial window and deliberately answers `false`.
    #[must_use]
    pub const fn is_rising(self) -> bool {
        matches!(self, Self::Rising { .. })
    }

    /// `true` for [`Advisory`](Self::Advisory): an early slope that has not
    /// collected a full window yet.
    #[must_use]
    pub const fn is_advisory(self) -> bool {
        matches!(self, Self::Advisory { .. })
    }
}

/// KiB per second implied by `growth_kb` over `intervals` samples spaced
/// `sample_interval_ms` apart. Saturates instead of overflowing: a caller
/// feeding absurd numbers gets `u64::MAX`, never a wrapped rate.
#[must_use]
pub const fn rate_kb_sec(growth_kb: u64, intervals: u64, sample_interval_ms: u64) -> u64 {
    let span_ms = (intervals as u128) * (sample_interval_ms as u128);
    if span_ms == 0 {
        return 0;
    }
    let rate = (growth_kb as u128) * 1_000 / span_ms;
    if rate > u64::MAX as u128 {
        u64::MAX
    } else {
        rate as u64
    }
}

/// Both halves of one memory verdict: where the process is ([`MemoryVerdict`])
/// and where it is going ([`LeakClassification`]).
///
/// One type on purpose. The level and the trend answer different questions,
/// and a verdict that reported only one of them hid the other: a `Soft`
/// level used to be handed back before the window was read, so a caller that
/// trusted the single value never saw the rising ring underneath. [`level`]
/// and [`trend`] are independent fields — read both, or use
/// [`is_pressured`](Self::is_pressured), which is true for either.
///
/// [`level`]: Self::level
/// [`trend`]: Self::trend
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoryReport {
    /// Level: `Ok` / `Soft` / `Hard` against the configured limits.
    pub level: MemoryVerdict,
    /// Trend: the shape of the sample window, against the trip mark.
    pub trend: LeakClassification,
}

impl Default for MemoryReport {
    fn default() -> Self {
        Self {
            level: MemoryVerdict::Ok,
            trend: LeakClassification::Warming { samples: 0 },
        }
    }
}

impl MemoryReport {
    /// `true` when this report refuses a new admission: a soft/hard level OR
    /// a full rising window over the trip mark.
    #[must_use]
    pub const fn is_pressured(self) -> bool {
        self.level.is_pressured() || self.trend.is_rising()
    }

    /// `true` only for the hard level.
    #[must_use]
    pub const fn is_hard(self) -> bool {
        self.level.is_hard()
    }

    /// `true` only for a full rising window that crossed the trip mark; a
    /// leak is UNDER both limits, which is why it is not a `MemoryVerdict`.
    #[must_use]
    pub const fn is_leaking(self) -> bool {
        self.trend.is_rising()
    }

    /// Static text for diagnostics: `"hard"` / `"soft"` / `"leaking"` /
    /// `"ok"` — the most urgent half wins.
    #[must_use]
    pub const fn code(self) -> &'static str {
        if self.level.is_hard() {
            "hard"
        } else if self.level.is_pressured() {
            "soft"
        } else if self.trend.is_rising() {
            "leaking"
        } else {
            "ok"
        }
    }

    /// Growth per second named by a rising window; `None` otherwise.
    #[must_use]
    pub const fn growth_rate_kb_sec(self) -> Option<u64> {
        match self.trend {
            LeakClassification::Rising { rate_kb_sec, .. } => rate_kb_sec,
            _ => None,
        }
    }
}

/// Level and trend in one cold-path value: [`MemoryWatch`] answers "over
/// budget yet", the ring answers "rising without a dip", and this owns both
/// so a caller keeps ONE structure and asks ONE question.
///
/// [`sample`](Self::sample) feeds the ring and hands back a [`MemoryReport`];
/// [`verdict`](Self::verdict) re-reads the same report without feeding. Both
/// halves are always computed — the level never masks the trend, because
/// neither is a substitute for the other.
///
/// ```
/// use snubber::{LeakClassification, MemoryTrend, MemoryVerdict};
///
/// let mut trend: MemoryTrend<4> = MemoryTrend::new(1_000, 2_000, 120, 500);
/// assert_eq!(trend.sample(100).code(), "ok", "one sample is not a slope");
/// assert!(!trend.sample(200).trend.is_advisory(), "two samples is not a slope");
/// // Three rising samples in a half-full window project past the mark:
/// // named early, and still not a latch.
/// assert_eq!(
///     trend.sample(300).trend,
///     LeakClassification::Advisory {
///         growth_kb: 200,
///         projected_growth_kb: 300,
///         samples: 3,
///         rate_kb_sec: Some(200),
///     }
/// );
/// assert!(!trend.verdict().is_leaking(), "an advisory never refuses");
/// assert_eq!(
///     trend.sample(500),
///     snubber::MemoryReport {
///         level: MemoryVerdict::Ok,
///         trend: LeakClassification::Rising { growth_kb: 400, rate_kb_sec: Some(266) },
///     },
///     "400 KiB over three 500 ms intervals"
/// );
/// let hard = trend.sample(2_100);
/// assert_eq!(hard.level, MemoryVerdict::Hard, "the level fires on its own");
/// assert!(hard.is_pressured());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoryTrend<const N: usize> {
    levels: MemoryWatch,
    ring: TrendRing<N>,
    trip_kb: u64,
    sample_interval_ms: u64,
}

impl<const N: usize> MemoryTrend<N> {
    /// Level limits, the leak trip mark (KiB of growth, `0` disables the
    /// leak verdict) and the caller's sampling interval (ms, `0` = unknown,
    /// so no rate is reported).
    #[must_use]
    pub const fn new(soft_kb: u64, hard_kb: u64, trip_kb: u64, sample_interval_ms: u64) -> Self {
        Self {
            levels: MemoryWatch::new(soft_kb, hard_kb),
            ring: TrendRing::new(),
            trip_kb,
            sample_interval_ms,
        }
    }

    /// Level limits only — the leak verdict is disabled.
    #[must_use]
    pub const fn levels_only(soft_kb: u64, hard_kb: u64) -> Self {
        Self::new(soft_kb, hard_kb, 0, 0)
    }

    /// Record one external sample (KiB) and return both halves for it.
    pub fn sample(&mut self, used_kb: u64) -> MemoryReport {
        let _ = self.levels.note(used_kb);
        self.ring.push(used_kb);
        self.verdict()
    }

    /// Both halves of the current state, without feeding the ring. The level
    /// and the trend are computed independently: a `Soft` level does not hide
    /// a rising window.
    #[must_use]
    pub fn verdict(&self) -> MemoryReport {
        MemoryReport {
            level: self.levels.verdict(),
            trend: self.ring.classify(self.trip_kb, self.sample_interval_ms),
        }
    }

    /// Growth per second over the current window, when it is full and rising.
    #[must_use]
    pub fn growth_rate_kb_sec(&self) -> Option<u64> {
        self.ring.growth_rate_kb_sec(self.sample_interval_ms)
    }

    /// Current window shape against the configured trip mark — the same value
    /// as [`verdict`](Self::verdict)`.trend`, for callers that only branch on
    /// the shape.
    #[must_use]
    pub fn classification(&self) -> LeakClassification {
        self.ring.classify(self.trip_kb, self.sample_interval_ms)
    }

    /// The level half, for callers that only need `used_kb` / `peak_kb`.
    #[must_use]
    pub const fn levels(&self) -> MemoryWatch {
        self.levels
    }

    /// Samples currently held in the window (`N` once it has wrapped).
    #[must_use]
    pub const fn samples(&self) -> usize {
        self.ring.len()
    }

    /// Newest sample in the window, KiB; `None` before the first sample.
    #[must_use]
    pub const fn newest_kb(&self) -> Option<u64> {
        self.ring.newest()
    }

    /// Configured leak trip mark, KiB (`0` = disabled).
    #[must_use]
    pub const fn trip_kb(&self) -> u64 {
        self.trip_kb
    }
}

// COMPILE-TIME GATE: 10 samples + two usizes (96 B on a 64-bit target,
// 88 B on a 32-bit one). The detector rides the same cold tick as the level
// watch and must stay a plain value the tick can keep on its stack.
const _: () = {
    const EXPECTED: usize = 10 * 8 + 2 * core::mem::size_of::<usize>();
    assert!(
        core::mem::size_of::<TrendRing<10>>() == EXPECTED,
        "CRITICAL: TrendRing<10> must stay 10 x u64 + 2 x usize"
    );
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// tests exercise the panic paths on purpose; the unwinds ARE the contract
mod tests {
    use super::*;

    #[test]
    fn verdicts_follow_limits_and_clamp_hard() {
        assert_eq!(MemoryWatch::OFF.verdict(), MemoryVerdict::Ok);

        let mut w = MemoryWatch::new(100, 200);
        assert_eq!(w.note(99), MemoryVerdict::Ok);
        assert_eq!(w.note(100), MemoryVerdict::Soft);
        assert_eq!(w.note(200), MemoryVerdict::Hard);
        assert_eq!(w.peak_kb(), 200);

        // hard < soft clamps up: the drain phase must exist.
        let clamped = MemoryWatch::new(100, 50);
        assert_eq!(clamped.hard_kb(), 100);

        // soft-only and hard-only nodes still work.
        let mut soft = MemoryWatch::new(10, 0);
        assert_eq!(soft.note(50), MemoryVerdict::Soft);
        let mut hard = MemoryWatch::new(0, 10);
        assert_eq!(hard.note(5), MemoryVerdict::Ok);
        assert_eq!(hard.note(10), MemoryVerdict::Hard);
    }

    #[test]
    fn verdict_codes_and_flags() {
        assert_eq!(MemoryVerdict::Ok.code(), "ok");
        assert_eq!(MemoryVerdict::Soft.code(), "soft");
        assert_eq!(MemoryVerdict::Hard.code(), "hard");
        assert!(!MemoryVerdict::Ok.is_pressured());
        assert!(MemoryVerdict::Soft.is_pressured());
        assert!(!MemoryVerdict::Soft.is_hard());
        assert!(MemoryVerdict::Hard.is_hard());
        assert!(!MemoryWatch::new(0, 0).is_configured());
        assert!(MemoryWatch::new(0, 1).is_configured());
    }

    #[test]
    fn trend_ring_reports_only_a_full_monotonic_window() {
        let mut w: TrendRing<4> = TrendRing::new();
        assert_eq!(w.window().samples, 0);
        // A short window is not evidence, however steep.
        assert_eq!(w.push(10), None);
        assert_eq!(w.push(1_000), None);
        assert_eq!(w.push(10_000), None);
        assert!(!w.window().full);
        assert_eq!(w.push(20_000), Some(19_990));
        assert!(w.window().full);
        assert_eq!(w.len(), 4);

        // The window keeps wrapping: oldest is evicted, growth is recomputed
        // over the new window, never accumulated.
        assert_eq!(w.push(30_000), Some(29_000));
    }

    #[test]
    fn trend_ring_clears_on_one_dip_and_needs_a_full_new_window() {
        let mut w: TrendRing<3> = TrendRing::new();
        assert_eq!(w.push(100), None);
        assert_eq!(w.push(200), None);
        assert_eq!(w.push(300), Some(200));

        // A dip is not a leak, and it empties the evidence: the next two
        // samples cannot revive a verdict even though both rise again.
        assert_eq!(w.push(250), None);
        assert!(w.window().full);
        assert_eq!(w.push(260), None);
        assert_eq!(w.push(270), Some(20), "a fresh, full rising window");

        // A plateau is not a leak either: equality must fail the same test
        // a dip does, or a flat working set reads as rising forever.
        let mut flat: TrendRing<3> = TrendRing::new();
        assert_eq!(flat.push(500), None);
        assert_eq!(flat.push(500), None);
        assert_eq!(flat.push(500), None);
        let mut flat_then_rise: TrendRing<3> = TrendRing::new();
        assert_eq!(flat_then_rise.push(500), None);
        assert_eq!(flat_then_rise.push(500), None);
        assert_eq!(
            flat_then_rise.push(501),
            None,
            "one equal neighbour clears it"
        );
    }

    #[test]
    fn trend_ring_of_zero_can_never_report() {
        let mut w: TrendRing<0> = TrendRing::new();
        assert_eq!(w.push(1), None);
        assert_eq!(w.push(2), None);
        assert_eq!(w.len(), 0);
        assert_eq!(w.window().samples, 0);
        assert!(!w.window().full);
    }

    #[test]
    fn window_reads_ends_and_shape_without_consuming_evidence() {
        let mut w: TrendRing<4> = TrendRing::new();
        assert_eq!(
            w.window(),
            TrendWindow {
                samples: 0,
                full: false,
                rising_partial: false,
                rising: false,
                oldest: 0,
                newest: 0
            }
        );

        // Partly filled: ends are real, a verdict is not — but a run with no
        // dip is readable as a slope.
        assert_eq!(w.push(10), None);
        assert_eq!(w.push(20), None);
        let half = w.window();
        assert_eq!((half.samples, half.oldest, half.newest), (2, 10, 20));
        assert!(!half.full && !half.rising);
        assert!(half.rising_partial);
        assert_eq!(half.growth_kb(), None, "two samples are not a trend");
        assert_eq!(
            half.partial_growth_kb(),
            Some(10),
            "the slope is real even when the verdict is not"
        );

        assert_eq!(w.push(30), None);
        assert_eq!(w.push(40), Some(30));
        let full = w.window();
        assert_eq!(
            (full.full, full.rising, full.growth_kb()),
            (true, true, Some(30))
        );
        assert_eq!(
            full.partial_growth_kb(),
            None,
            "a full window answers through growth_kb, not the partial slope"
        );
        assert_eq!((full.oldest, full.newest), (10, 40));

        // Reading the window twice must not move the ring.
        assert_eq!(w.window(), full);
        assert_eq!(w.len(), 4);
    }

    #[test]
    fn growth_rate_divides_by_the_span_the_caller_owns() {
        let mut w: TrendRing<4> = TrendRing::new();
        for sample in [1_000u64, 2_000, 3_000, 4_000] {
            w.push(sample);
        }
        // 3000 KiB over three intervals of 500 ms = 1.5 s.
        assert_eq!(w.growth_rate_kb_sec(500), Some(2_000));
        assert_eq!(w.growth_rate_kb_sec(1_000), Some(1_000));
        // No interval, or a window that spans no time, names no rate.
        assert_eq!(w.growth_rate_kb_sec(0), None);
        let mut one: TrendRing<1> = TrendRing::new();
        one.push(7);
        assert_eq!(one.growth_rate_kb_sec(500), None, "N < 2 spans no time");

        // A dip clears the rate with the verdict.
        let mut dipped: TrendRing<3> = TrendRing::new();
        dipped.push(10);
        dipped.push(20);
        dipped.push(19);
        assert_eq!(dipped.growth_rate_kb_sec(500), None);
    }

    #[test]
    fn classification_names_every_shape() {
        let mut warming: TrendRing<3> = TrendRing::new();
        warming.push(1);
        warming.push(2);
        assert_eq!(
            warming.classify(1, 500),
            LeakClassification::Warming { samples: 2 }
        );

        let mut steady: TrendRing<3> = TrendRing::new();
        for sample in [100u64, 150, 200] {
            steady.push(sample);
        }
        assert_eq!(
            steady.classify(120, 500),
            LeakClassification::Stable { growth_kb: 100 }
        );

        let mut rising: TrendRing<3> = TrendRing::new();
        for sample in [100u64, 200, 300] {
            rising.push(sample);
        }
        assert_eq!(
            rising.classify(120, 500),
            LeakClassification::Rising {
                growth_kb: 200,
                rate_kb_sec: Some(200),
            },
            "200 KiB over two 500 ms intervals"
        );
        assert_eq!(
            rising.classify(120, 0),
            LeakClassification::Rising {
                growth_kb: 200,
                rate_kb_sec: None,
            },
            "no interval, no rate — the shape still stands"
        );
        assert!(rising.classify(120, 500).is_rising());

        // The sawtooth: a real leak that the monotonic trip can never see.
        let mut saw: TrendRing<4> = TrendRing::new();
        for sample in [100u64, 400, 200, 500] {
            saw.push(sample);
        }
        assert_eq!(saw.push(600), None, "the dip is still in the window");
        assert_eq!(saw.classify(120, 500), LeakClassification::Sawtooth);
        assert_eq!(saw.classify(120, 500).code(), "sawtooth");
    }

    #[test]
    fn advisory_names_a_steep_partial_window_early_and_never_latches() {
        let mut w: TrendRing<6> = TrendRing::new();
        for sample in [100u64, 200, 300] {
            w.push(sample);
        }
        assert_eq!(
            w.classify(120, 500),
            LeakClassification::Advisory {
                growth_kb: 200,
                projected_growth_kb: 500,
                samples: 3,
                rate_kb_sec: Some(200),
            },
            "200 KiB over two intervals, projected across five"
        );
        let advisory = w.classify(120, 500);
        assert!(advisory.is_advisory());
        assert!(!advisory.is_rising(), "an advisory is not a leak verdict");
        assert_eq!(advisory.code(), "advisory");
        assert_eq!(
            w.classify(120, 0),
            LeakClassification::Advisory {
                growth_kb: 200,
                projected_growth_kb: 500,
                samples: 3,
                rate_kb_sec: None,
            },
            "no interval, no rate — the slope still stands"
        );

        // The ring owner agrees: an advisory never refuses an admission.
        let mut trend: MemoryTrend<6> = MemoryTrend::new(1_000, 2_000, 120, 500);
        for sample in [100u64, 200, 300] {
            let report = trend.sample(sample);
            assert!(!report.is_leaking() && !report.is_pressured());
            assert_eq!(report.code(), "ok");
            assert_eq!(report.growth_rate_kb_sec(), None);
        }
        assert!(trend.classification().is_advisory());

        // When the window fills and stayed rising, the full verdict fires.
        for sample in [400u64, 500, 600] {
            trend.sample(sample);
        }
        assert_eq!(
            trend.classification(),
            LeakClassification::Rising {
                growth_kb: 500,
                rate_kb_sec: Some(200),
            }
        );
        assert!(trend.verdict().is_leaking());
    }

    #[test]
    fn advisory_needs_three_samples_and_at_least_half_a_window() {
        // N = 10: four rising samples project past the mark, but half a
        // window is five, so the slope is not named yet.
        let mut narrow: TrendRing<10> = TrendRing::new();
        for sample in [100u64, 200, 300, 400] {
            narrow.push(sample);
        }
        assert_eq!(
            narrow.classify(100, 500),
            LeakClassification::Warming { samples: 4 },
            "4 of 10 samples are not half a window"
        );
        narrow.push(500);
        assert_eq!(
            narrow.classify(100, 500),
            LeakClassification::Advisory {
                growth_kb: 400,
                projected_growth_kb: 900,
                samples: 5,
                rate_kb_sec: Some(200),
            },
            "5 of 10 samples are, and the projection crosses 100"
        );

        // Two samples is a tick and a half — never a trend, however steep.
        let mut two: TrendRing<10> = TrendRing::new();
        two.push(1);
        two.push(1_000_000);
        assert_eq!(
            two.classify(1, 500),
            LeakClassification::Warming { samples: 2 }
        );

        // A slope that would NOT cross the mark stays warming.
        let mut calm: TrendRing<6> = TrendRing::new();
        for sample in [100u64, 101, 102] {
            calm.push(sample);
        }
        assert_eq!(
            calm.classify(1_000, 500),
            LeakClassification::Warming { samples: 3 }
        );

        // Trip 0 disables the verdict, and the advisory with it.
        assert_eq!(
            calm.classify(0, 500),
            LeakClassification::Warming { samples: 3 }
        );

        // A dip or a plateau inside the held samples clears the slope.
        let mut dipped: TrendRing<6> = TrendRing::new();
        for sample in [100u64, 200, 150] {
            dipped.push(sample);
        }
        assert_eq!(
            dipped.classify(120, 500),
            LeakClassification::Warming { samples: 3 }
        );
        let mut flat: TrendRing<6> = TrendRing::new();
        for sample in [100u64, 100, 100] {
            flat.push(sample);
        }
        assert_eq!(
            flat.classify(120, 500),
            LeakClassification::Warming { samples: 3 }
        );

        // A window narrower than the minimum sample count can never advise.
        let mut tiny: TrendRing<2> = TrendRing::new();
        tiny.push(1);
        assert_eq!(
            tiny.classify(1, 500),
            LeakClassification::Warming { samples: 1 }
        );
    }

    #[test]
    fn project_growth_extrapolates_and_saturates() {
        assert_eq!(project_growth(10, 2, 3), 20);
        assert_eq!(project_growth(200, 3, 4), 300);
        assert_eq!(project_growth(200, 4, 10), 600);
        assert_eq!(
            project_growth(u64::MAX, 2, 3),
            u64::MAX,
            "absurd growth saturates instead of wrapping"
        );
        // A projection over a shape it cannot widen is the growth itself.
        assert_eq!(project_growth(10, 1, 3), 10);
        assert_eq!(project_growth(10, 3, 0), 10);
        assert_eq!(project_growth(10, 5, 4), 10);
    }

    #[test]
    fn rate_saturates_instead_of_wrapping() {
        assert_eq!(rate_kb_sec(1_000, 3, 500), 666);
        assert_eq!(rate_kb_sec(0, 3, 500), 0);
        assert_eq!(rate_kb_sec(1_000, 0, 500), 0, "a zero span names no rate");
        assert_eq!(
            rate_kb_sec(u64::MAX, 1, 1),
            u64::MAX,
            "u64::MAX KiB in one millisecond cannot wrap into a small number"
        );
    }

    #[test]
    fn verdict_carries_both_halves_and_never_masks_the_trend() {
        let mut trend: MemoryTrend<3> = MemoryTrend::new(1_000, 2_000, 120, 500);
        assert_eq!(
            trend.sample(100).trend,
            LeakClassification::Warming { samples: 1 }
        );
        assert_eq!(trend.sample(200).code(), "ok");
        let leak = trend.sample(300);
        assert_eq!(
            leak,
            MemoryReport {
                level: MemoryVerdict::Ok,
                trend: LeakClassification::Rising {
                    growth_kb: 200,
                    rate_kb_sec: Some(200),
                },
            }
        );
        assert!(leak.is_pressured(), "a leak refuses admissions");
        assert!(leak.is_leaking());
        assert!(!leak.is_hard());
        assert_eq!(leak.code(), "leaking");
        assert_eq!(leak.growth_rate_kb_sec(), Some(200));
        assert_eq!(trend.levels().used_kb(), 300);
        assert_eq!(trend.trip_kb(), 120);

        // The level fires on its own, and the trend stays readable: a Soft
        // report over a still-rising ring shows BOTH halves (0.7 masked the
        // trend behind the level; that is the trap this test pins).
        let soft = trend.sample(1_500);
        assert_eq!(soft.level, MemoryVerdict::Soft);
        assert_eq!(
            soft.trend,
            LeakClassification::Rising {
                growth_kb: 1_300,
                rate_kb_sec: Some(1_300),
            },
            "a level episode must not blind the window"
        );
        assert_eq!(soft.code(), "soft");
        assert_eq!(trend.sample(2_500).level, MemoryVerdict::Hard);
        assert_eq!(trend.levels().peak_kb(), 2_500);

        // `verdict()` reads without feeding: the same report twice.
        let read = trend.verdict();
        assert_eq!(read, trend.verdict());
        assert_eq!(trend.samples(), 3);
        assert_eq!(trend.newest_kb(), Some(2_500));

        // A level-only trend can never report a leak.
        let mut levels: MemoryTrend<3> = MemoryTrend::levels_only(1_000, 2_000);
        for sample in [10u64, 20, 30, 40] {
            assert!(!levels.sample(sample).is_leaking());
        }
        let soft_only = levels.sample(1_001);
        assert_eq!(
            soft_only.level,
            MemoryVerdict::Soft,
            "limits still fire with the leak verdict disabled"
        );
        assert!(
            !soft_only.is_leaking(),
            "trip 0 disables the verdict even over a rising window"
        );
    }

    #[test]
    fn report_defaults_and_flags_cover_every_half() {
        let ok = MemoryReport::default();
        assert_eq!(ok.level, MemoryVerdict::Ok);
        assert_eq!(ok.trend, LeakClassification::Warming { samples: 0 });
        assert!(!ok.is_pressured());
        assert_eq!(ok.code(), "ok");
        assert_eq!(ok.growth_rate_kb_sec(), None);

        let hard = MemoryReport {
            level: MemoryVerdict::Hard,
            trend: LeakClassification::Rising {
                growth_kb: 400,
                rate_kb_sec: Some(42),
            },
        };
        assert!(hard.is_hard());
        assert!(hard.is_pressured());
        assert_eq!(hard.code(), "hard", "the level outranks the trend text");
        assert_eq!(hard.growth_rate_kb_sec(), Some(42));

        let soft = MemoryReport {
            level: MemoryVerdict::Soft,
            ..MemoryReport::default()
        };
        assert_eq!(soft.code(), "soft");
        assert!(soft.is_pressured());
        assert!(!soft.is_leaking());

        let leaking = MemoryReport {
            trend: LeakClassification::Rising {
                growth_kb: 400,
                rate_kb_sec: None,
            },
            ..MemoryReport::default()
        };
        assert_eq!(leaking.code(), "leaking");
        assert_eq!(
            leaking.growth_rate_kb_sec(),
            None,
            "no interval, no rate — the verdict still stands"
        );
    }
}
