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

/// Verdict for one **[`MemoryTrend`] sample**: a level verdict, or a leak.
///
/// A separate type on purpose. `MemoryVerdict` is a public enum with no
/// `repr`, so a field-carrying variant would leave its discriminants
/// unspecified — a major change for anyone who casts or matches on it.
/// Composing instead of extending keeps `MemoryVerdict` exactly as it was:
/// `level()` always yields a value of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MemoryTrendVerdict {
    /// `used < soft` and no leak: admit normally.
    #[default]
    Ok,
    /// `used >= soft_kb`.
    Soft,
    /// `used >= hard_kb`.
    Hard,
    /// Under both limits, but a full window rose without one dip by more
    /// than the trip mark: refuse new admissions until the window goes
    /// quiet. A level says where the process is, this says where it is
    /// going; a healthy process may sit near the soft limit forever, a
    /// leaking one crosses it eventually.
    Leaking {
        /// Growth per second over the window (`0` when the caller armed no
        /// sample interval, so no rate could be computed).
        growth_rate_kb_sec: u64,
    },
}

impl MemoryTrendVerdict {
    /// `true` for every verdict but [`Ok`](Self::Ok): soft, hard and leaking
    /// all refuse a new admission.
    #[must_use]
    pub const fn is_pressured(self) -> bool {
        !matches!(self, Self::Ok)
    }

    /// `true` only for [`Hard`](Self::Hard).
    #[must_use]
    pub const fn is_hard(self) -> bool {
        matches!(self, Self::Hard)
    }

    /// `true` only for [`Leaking`](Self::Leaking).
    #[must_use]
    pub const fn is_leaking(self) -> bool {
        matches!(self, Self::Leaking { .. })
    }

    /// Growth named by [`Leaking`](Self::Leaking), KiB/s; `None` otherwise.
    #[must_use]
    pub const fn growth_rate_kb_sec(self) -> Option<u64> {
        match self {
            Self::Leaking { growth_rate_kb_sec } => Some(growth_rate_kb_sec),
            _ => None,
        }
    }

    /// The level half of this verdict; a leak is `Ok` for the level, which
    /// is the truth: the process is under both limits.
    #[must_use]
    pub const fn level(self) -> MemoryVerdict {
        match self {
            Self::Soft => MemoryVerdict::Soft,
            Self::Hard => MemoryVerdict::Hard,
            Self::Ok | Self::Leaking { .. } => MemoryVerdict::Ok,
        }
    }

    /// Static text for diagnostics (`"ok"` / `"soft"` / `"hard"` /
    /// `"leaking"`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Soft => "soft",
            Self::Hard => "hard",
            Self::Leaking { .. } => "leaking",
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

/// A leak is a DERIVATIVE, not a level: [`MemoryWatch`] answers "over
/// budget yet", this answers "rising without a single dip".
///
/// One ring of `N` samples, oldest-overwritten. A verdict exists only when
/// the ring is full AND every sample is strictly greater than the one
/// before it; the growth is then `newest - oldest`. One sample that fails
/// to beat its predecessor clears the window, because a healthy process is
/// allowed to oscillate (a collection, a tray collapse, a pool draining).
///
/// The crate still does not measure: the caller feeds one external sample
/// per tick, exactly like `note_memory_kb`. `N` is the caller's choice —
/// ten 30 s samples are five minutes of evidence; the number belongs to the
/// policy, not to the detector.
///
/// ```
/// use snubber::LeakWatch;
///
/// let mut watch: LeakWatch<4> = LeakWatch::new();
/// assert_eq!(watch.push(100), None, "a partial window says nothing");
/// assert_eq!(watch.push(200), None);
/// assert_eq!(watch.push(300), None);
/// assert_eq!(watch.push(420), Some(320), "full, strictly rising: 420 - 100");
/// assert_eq!(watch.push(400), None, "one dip clears the window");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeakWatch<const N: usize> {
    ring: [u64; N],
    len: usize,
    next: usize,
}

impl<const N: usize> LeakWatch<N> {
    /// Empty window. `N = 0` is a watch that can never report a leak.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ring: [0; N],
            len: 0,
            next: 0,
        }
    }

    /// Push one sample and return the growth of a strictly rising FULL
    /// window (`newest - oldest`), or `None` while the window is short or
    /// not monotonic.
    pub fn push(&mut self, sample: u64) -> Option<u64> {
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
    pub fn window(&self) -> LeakWindow {
        if N == 0 || self.len == 0 {
            return LeakWindow {
                samples: 0,
                full: false,
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
        let mut rising = self.len == N;
        if rising {
            let mut previous = oldest;
            for i in 1..N {
                let sample = self.ring[(self.next + i) % N];
                if sample <= previous {
                    rising = false;
                    break;
                }
                previous = sample;
            }
        }
        LeakWindow {
            samples: self.len,
            full: self.len == N,
            rising,
            oldest,
            newest,
        }
    }

    /// Growth per second over a full rising window, given the caller's tick.
    /// `None` while the window is short, not monotonic, or the interval or
    /// the window width cannot span time (`N < 2`, `sample_interval_ms == 0`).
    #[must_use]
    pub fn growth_rate_kb_sec(&self, sample_interval_ms: u64) -> Option<u64> {
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
    pub fn classify(&self, trip_kb: u64, sample_interval_ms: u64) -> LeakClassification {
        let window = self.window();
        match window.growth_kb() {
            Some(growth) if growth_crosses(growth, trip_kb) => LeakClassification::Rising {
                growth_kb: growth,
                rate_kb_sec: self.growth_rate_kb_sec(sample_interval_ms),
            },
            Some(growth) => LeakClassification::Stable { growth_kb: growth },
            None if window.full => LeakClassification::Sawtooth,
            None => LeakClassification::Warming {
                samples: window.samples,
            },
        }
    }

    /// Samples currently in the window (`N` once it has wrapped).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` while more samples are needed before a verdict is possible.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `true` once the window is full and a verdict is possible at all.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == N && N != 0
    }
}

/// One read of a [`LeakWatch`] window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LeakWindow {
    /// Samples held (`N` once the ring has wrapped).
    pub samples: usize,
    /// `true` when the ring holds `N` samples: a verdict is possible.
    pub full: bool,
    /// `true` only for a full window whose every sample beat the one before
    /// it. Equality counts as a dip, like a fall does.
    pub rising: bool,
    /// Oldest sample in the window, KiB.
    pub oldest: u64,
    /// Newest sample in the window, KiB.
    pub newest: u64,
}

impl LeakWindow {
    /// `newest - oldest` when the window is full and rising; `None` otherwise.
    /// A partial window can look steep and still say nothing — one more
    /// sample may be the dip that ends it.
    #[must_use]
    pub const fn growth_kb(self) -> Option<u64> {
        if self.full && self.rising {
            Some(self.newest - self.oldest)
        } else {
            None
        }
    }
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
}

impl LeakClassification {
    /// Static text for diagnostics (`"warming"` / `"stable"` / `"rising"` /
    /// `"sawtooth"`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Warming { .. } => "warming",
            Self::Stable { .. } => "stable",
            Self::Rising { .. } => "rising",
            Self::Sawtooth => "sawtooth",
        }
    }

    /// `true` only for [`Rising`](Self::Rising).
    #[must_use]
    pub const fn is_rising(self) -> bool {
        matches!(self, Self::Rising { .. })
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

/// Level and trend in one cold-path value: [`MemoryWatch`] answers "over
/// budget yet", [`LeakWatch`] answers "rising without a dip", and this
/// composes them so a caller keeps ONE ring and gets ONE verdict.
///
/// The level outranks the trend: `Hard` is not softened into `Leaking`, and
/// `Soft` stays `Soft` while the window is still warming. The trend is fed
/// on every sample regardless, so a level episode does not blind the ring.
///
/// ```
/// use snubber::{MemoryTrend, MemoryTrendVerdict, MemoryVerdict};
///
/// let mut trend: MemoryTrend<4> = MemoryTrend::new(1_000, 2_000, 120, 500);
/// assert_eq!(trend.sample(100), MemoryTrendVerdict::Ok, "a short window says nothing");
/// assert_eq!(trend.sample(200), MemoryTrendVerdict::Ok);
/// assert_eq!(trend.sample(300), MemoryTrendVerdict::Ok);
/// assert_eq!(
///     trend.sample(500),
///     MemoryTrendVerdict::Leaking { growth_rate_kb_sec: 266 },
///     "400 KiB over three 500 ms intervals"
/// );
/// let hard = trend.sample(2_100);
/// assert_eq!(hard, MemoryTrendVerdict::Hard, "the level outranks the trend");
/// assert_eq!(hard.level(), MemoryVerdict::Hard);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoryTrend<const N: usize> {
    levels: MemoryWatch,
    leaks: LeakWatch<N>,
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
            leaks: LeakWatch::new(),
            trip_kb,
            sample_interval_ms,
        }
    }

    /// Level limits only — the leak verdict is disabled.
    #[must_use]
    pub const fn levels_only(soft_kb: u64, hard_kb: u64) -> Self {
        Self::new(soft_kb, hard_kb, 0, 0)
    }

    /// Record one external sample (KiB) and return the verdict for it. The
    /// level is reported first; the ring is fed either way.
    pub fn sample(&mut self, used_kb: u64) -> MemoryTrendVerdict {
        let level = self.levels.note(used_kb);
        let growth = self.leaks.push(used_kb);
        if level.is_hard() {
            return MemoryTrendVerdict::Hard;
        }
        if level.is_pressured() {
            return MemoryTrendVerdict::Soft;
        }
        match growth {
            Some(growth) if growth_crosses(growth, self.trip_kb) => MemoryTrendVerdict::Leaking {
                growth_rate_kb_sec: self.rate_kb_sec(growth),
            },
            _ => MemoryTrendVerdict::Ok,
        }
    }

    /// Growth per second over the current window, when it is full and rising.
    #[must_use]
    pub fn growth_rate_kb_sec(&self) -> Option<u64> {
        self.leaks.growth_rate_kb_sec(self.sample_interval_ms)
    }

    /// Current window shape against the configured trip mark.
    #[must_use]
    pub fn classification(&self) -> LeakClassification {
        self.leaks.classify(self.trip_kb, self.sample_interval_ms)
    }

    /// The level half, for callers that only need `used_kb` / `peak_kb`.
    #[must_use]
    pub const fn levels(&self) -> MemoryWatch {
        self.levels
    }

    /// The trend half, for callers that scan the raw window themselves.
    #[must_use]
    pub const fn leaks(&self) -> &LeakWatch<N> {
        &self.leaks
    }

    /// Configured leak trip mark, KiB (`0` = disabled).
    #[must_use]
    pub const fn trip_kb(&self) -> u64 {
        self.trip_kb
    }

    fn rate_kb_sec(&self, growth_kb: u64) -> u64 {
        if N < 2 || self.sample_interval_ms == 0 {
            return 0;
        }
        rate_kb_sec(growth_kb, N as u64 - 1, self.sample_interval_ms)
    }
}

impl<const N: usize> Default for LeakWatch<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Does a monotonic window that grew this much name a leak? `trip_kb == 0`
/// disables the verdict.
#[must_use]
pub const fn growth_crosses(growth_kb: u64, trip_kb: u64) -> bool {
    trip_kb > 0 && growth_kb >= trip_kb
}

// COMPILE-TIME GATE: 10 samples + two usizes (96 B on a 64-bit target,
// 88 B on a 32-bit one). The detector rides the same cold tick as the level
// watch and must stay a plain value the tick can keep on its stack.
const _: () = {
    const EXPECTED: usize = 10 * 8 + 2 * core::mem::size_of::<usize>();
    assert!(
        core::mem::size_of::<LeakWatch<10>>() == EXPECTED,
        "CRITICAL: LeakWatch<10> must stay 10 x u64 + 2 x usize"
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
    fn leak_watch_reports_only_a_full_monotonic_window() {
        let mut w: LeakWatch<4> = LeakWatch::new();
        assert!(w.is_empty());
        // A short window is not evidence, however steep.
        assert_eq!(w.push(10), None);
        assert_eq!(w.push(1_000), None);
        assert_eq!(w.push(10_000), None);
        assert!(!w.is_full());
        assert_eq!(w.push(20_000), Some(19_990));
        assert!(w.is_full());
        assert_eq!(w.len(), 4);

        // The window keeps wrapping: oldest is evicted, growth is recomputed
        // over the new window, never accumulated.
        assert_eq!(w.push(30_000), Some(29_000));
    }

    #[test]
    fn leak_watch_clears_on_one_dip_and_needs_a_full_new_window() {
        let mut w: LeakWatch<3> = LeakWatch::new();
        assert_eq!(w.push(100), None);
        assert_eq!(w.push(200), None);
        assert_eq!(w.push(300), Some(200));

        // A dip is not a leak, and it empties the evidence: the next two
        // samples cannot revive a verdict even though both rise again.
        assert_eq!(w.push(250), None);
        assert!(w.is_full());
        assert_eq!(w.push(260), None);
        assert_eq!(w.push(270), Some(20), "a fresh, full rising window");

        // A plateau is not a leak either: equality must fail the same test
        // a dip does, or a flat working set reads as rising forever.
        let mut flat: LeakWatch<3> = LeakWatch::new();
        assert_eq!(flat.push(500), None);
        assert_eq!(flat.push(500), None);
        assert_eq!(flat.push(500), None);
        let mut flat_then_rise: LeakWatch<3> = LeakWatch::new();
        assert_eq!(flat_then_rise.push(500), None);
        assert_eq!(flat_then_rise.push(500), None);
        assert_eq!(
            flat_then_rise.push(501),
            None,
            "one equal neighbour clears it"
        );
    }

    #[test]
    fn leak_watch_of_zero_can_never_report() {
        let mut w: LeakWatch<0> = LeakWatch::new();
        assert_eq!(w.push(1), None);
        assert_eq!(w.push(2), None);
        assert_eq!(w.len(), 0);
        assert!(w.is_empty());
        assert!(!w.is_full());
    }

    #[test]
    fn growth_crosses_is_the_trip_mark_not_a_level() {
        // The mark is a GROWTH: same absolute number, different verdicts.
        assert!(!growth_crosses(0, 120));
        assert!(!growth_crosses(119, 120));
        assert!(growth_crosses(120, 120));
        assert!(growth_crosses(4_096, 120));
        // Zero disables the verdict rather than tripping on everything.
        assert!(!growth_crosses(1_000_000, 0));
    }

    #[test]
    fn leak_watch_feeds_growth_crosses() {
        let mut w: LeakWatch<3> = LeakWatch::new();
        let mut verdicts = [None; 4];
        for (slot, sample) in verdicts.iter_mut().zip([1_000u64, 2_000, 3_000, 2_999]) {
            *slot = w.push(sample).filter(|&growth| growth_crosses(growth, 120));
        }
        assert_eq!(
            verdicts,
            [None, None, Some(2_000), None],
            "3000 - 1000 crosses, one dip below 3000 clears the window"
        );
    }

    #[test]
    fn window_reads_ends_and_shape_without_consuming_evidence() {
        let mut w: LeakWatch<4> = LeakWatch::new();
        assert_eq!(
            w.window(),
            LeakWindow {
                samples: 0,
                full: false,
                rising: false,
                oldest: 0,
                newest: 0
            }
        );

        // Partly filled: ends are real, a verdict is not.
        assert_eq!(w.push(10), None);
        assert_eq!(w.push(20), None);
        let half = w.window();
        assert_eq!((half.samples, half.oldest, half.newest), (2, 10, 20));
        assert!(!half.full && !half.rising);
        assert_eq!(half.growth_kb(), None, "two samples are not a trend");

        assert_eq!(w.push(30), None);
        assert_eq!(w.push(40), Some(30));
        let full = w.window();
        assert_eq!(
            (full.full, full.rising, full.growth_kb()),
            (true, true, Some(30))
        );
        assert_eq!((full.oldest, full.newest), (10, 40));

        // Reading the window twice must not move the ring.
        assert_eq!(w.window(), full);
        assert_eq!(w.len(), 4);
    }

    #[test]
    fn growth_rate_divides_by_the_span_the_caller_owns() {
        let mut w: LeakWatch<4> = LeakWatch::new();
        for sample in [1_000u64, 2_000, 3_000, 4_000] {
            w.push(sample);
        }
        // 3000 KiB over three intervals of 500 ms = 1.5 s.
        assert_eq!(w.growth_rate_kb_sec(500), Some(2_000));
        assert_eq!(w.growth_rate_kb_sec(1_000), Some(1_000));
        // No interval, or a window that spans no time, names no rate.
        assert_eq!(w.growth_rate_kb_sec(0), None);
        let mut one: LeakWatch<1> = LeakWatch::new();
        one.push(7);
        assert_eq!(one.growth_rate_kb_sec(500), None, "N < 2 spans no time");

        // A dip clears the rate with the verdict.
        let mut dipped: LeakWatch<3> = LeakWatch::new();
        dipped.push(10);
        dipped.push(20);
        dipped.push(19);
        assert_eq!(dipped.growth_rate_kb_sec(500), None);
    }

    #[test]
    fn classification_names_every_shape() {
        let mut warming: LeakWatch<3> = LeakWatch::new();
        warming.push(1);
        warming.push(2);
        assert_eq!(
            warming.classify(1, 500),
            LeakClassification::Warming { samples: 2 }
        );

        let mut steady: LeakWatch<3> = LeakWatch::new();
        for sample in [100u64, 150, 200] {
            steady.push(sample);
        }
        assert_eq!(
            steady.classify(120, 500),
            LeakClassification::Stable { growth_kb: 100 }
        );

        let mut rising: LeakWatch<3> = LeakWatch::new();
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
        let mut saw: LeakWatch<4> = LeakWatch::new();
        for sample in [100u64, 400, 200, 500] {
            saw.push(sample);
        }
        assert_eq!(saw.push(600), None, "the dip is still in the window");
        assert_eq!(saw.classify(120, 500), LeakClassification::Sawtooth);
        assert_eq!(saw.classify(120, 500).code(), "sawtooth");
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
    fn trend_puts_the_level_above_the_verdict() {
        let mut trend: MemoryTrend<3> = MemoryTrend::new(1_000, 2_000, 120, 500);
        assert_eq!(trend.sample(100), MemoryTrendVerdict::Ok);
        assert_eq!(trend.sample(200), MemoryTrendVerdict::Ok);
        assert_eq!(
            trend.sample(300),
            MemoryTrendVerdict::Leaking {
                growth_rate_kb_sec: 200
            }
        );
        assert!(trend.classification().is_rising());
        assert_eq!(trend.levels().used_kb(), 300);
        assert_eq!(trend.trip_kb(), 120);

        // The level outranks: Soft is never reported as Leaking.
        assert_eq!(trend.sample(1_500), MemoryTrendVerdict::Soft);
        assert_eq!(trend.sample(2_500), MemoryTrendVerdict::Hard);
        assert_eq!(trend.levels().peak_kb(), 2_500);

        // The ring kept sampling through the level episode: it still sees.
        let mut blind: MemoryTrend<3> = MemoryTrend::new(1_000, 2_000, 0, 500);
        for sample in [10u64, 20, 30] {
            assert_eq!(blind.sample(sample), MemoryTrendVerdict::Ok);
        }
        assert_eq!(
            blind.classification(),
            LeakClassification::Stable { growth_kb: 20 },
            "trip 0 disables the verdict, not the ring"
        );
    }

    #[test]
    fn leaking_verdict_carries_its_rate_and_flags() {
        let leaking = MemoryTrendVerdict::Leaking {
            growth_rate_kb_sec: 42,
        };
        assert_eq!(leaking.code(), "leaking");
        assert!(leaking.is_pressured(), "a leak refuses admissions");
        assert!(leaking.is_leaking());
        assert!(!leaking.is_hard());
        assert_eq!(leaking.growth_rate_kb_sec(), Some(42));
        assert_eq!(
            leaking.level(),
            MemoryVerdict::Ok,
            "a leak is under both limits; the level half says so"
        );
        assert_eq!(MemoryTrendVerdict::Ok.growth_rate_kb_sec(), None);
        assert_eq!(MemoryTrendVerdict::Soft.growth_rate_kb_sec(), None);
        assert!(!MemoryTrendVerdict::Soft.is_leaking());
        assert!(!MemoryTrendVerdict::Ok.is_pressured());
        assert_eq!(MemoryTrendVerdict::Soft.level(), MemoryVerdict::Soft);
        assert_eq!(MemoryTrendVerdict::Hard.level(), MemoryVerdict::Hard);

        // A level-only trend can never report a leak.
        let mut levels: MemoryTrend<3> = MemoryTrend::levels_only(1_000, 2_000);
        for sample in [10u64, 20, 30, 40] {
            assert!(!levels.sample(sample).is_leaking());
        }
        assert_eq!(
            levels.sample(1_001),
            MemoryTrendVerdict::Soft,
            "limits still fire with the trend disabled"
        );
    }
}
