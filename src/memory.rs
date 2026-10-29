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
        if self.len < N {
            return None;
        }
        let oldest = self.ring[self.next];
        let mut previous = oldest;
        for i in 1..N {
            let sample = self.ring[(self.next + i) % N];
            if sample <= previous {
                return None;
            }
            previous = sample;
        }
        Some(previous - oldest)
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
}
