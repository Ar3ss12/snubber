//! Handle / descriptor growth: the same derivative rule as a memory leak,
//! applied to a count the crate itself never reads.
//!
//! A leaked handle is invisible to an RSS watch — a `HANDLE`, a file
//! descriptor or a socket is a few bytes of kernel state, not resident
//! pages — so the only honest detector is the derivative of the count.
//! [`HandleWatch`] holds that rule and nothing else: no sampler, no OS call,
//! no clock. The caller passes a probe, exactly like
//! [`SnubberNode::note_memory_kb`](crate::SnubberNode::note_memory_kb)
//! passes a sample.
//!
//! A probe that cannot read its count returns `None`, and **no sample is
//! recorded**: feeding `0` instead would be a dip that clears the window and
//! silently disarms the detector on the one platform whose probe failed.
//!
//! The window may also answer [`LeakClassification::Advisory`] — a partial
//! window that rose without a dip and would cross the trip mark by the time
//! it fills. It is an early slope, never a latch: [`HandleWatch::is_leaking`]
//! stays `false` for it, exactly as [`MemoryReport::is_leaking`] does.
//!
//! [`MemoryReport::is_leaking`]: crate::MemoryReport::is_leaking

use crate::memory::{LeakClassification, TrendRing};

/// Monotonic window over an externally sampled handle count.
///
/// ```
/// use snubber::{HandleWatch, LeakClassification};
///
/// let mut watch: HandleWatch<3> = HandleWatch::new(8, 1_000);
/// let mut early = || Some(100);
/// watch.sample(&mut early);
/// let mut next = || Some(120);
/// watch.sample(&mut next);
///
/// let mut climbed = || Some(300);
/// assert_eq!(
///     watch.sample(&mut climbed).map(LeakClassification::code),
///     Some("rising"),
///     "+200 handles over a rising window crosses the mark"
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandleWatch<const N: usize> {
    ring: TrendRing<N>,
    trip: u64,
    sample_interval_ms: u64,
}

impl<const N: usize> HandleWatch<N> {
    /// `trip` = growth over the window that counts as a leak (`0` disables
    /// the verdict), `sample_interval_ms` = the caller's tick (`0` = unknown,
    /// so no rate is reported).
    #[must_use]
    pub const fn new(trip: u64, sample_interval_ms: u64) -> Self {
        Self {
            ring: TrendRing::new(),
            trip,
            sample_interval_ms,
        }
    }

    /// Sample the count once through a caller-supplied probe.
    ///
    /// `None` from the probe means "this platform cannot read a count right
    /// now": nothing is recorded and the caller learns nothing new. An
    /// unreadable sample must never look like a dip.
    pub fn sample<S: FnMut() -> Option<u64>>(
        &mut self,
        probe: &mut S,
    ) -> Option<LeakClassification> {
        let handles = probe()?;
        Some(self.note(handles))
    }

    /// Record one count and return the window's shape. For callers that
    /// already have the number (a snapshot, a test, a replay).
    pub fn note(&mut self, handles: u64) -> LeakClassification {
        self.ring.push(handles);
        self.classification()
    }

    /// Current shape against the configured trip mark.
    #[must_use]
    pub fn classification(&self) -> LeakClassification {
        self.ring.classify(self.trip, self.sample_interval_ms)
    }

    /// `true` only for a full, rising window that crossed the trip mark.
    #[must_use]
    pub fn is_leaking(&self) -> bool {
        self.classification().is_rising()
    }

    /// Handle growth per second over the window; `None` while the window is
    /// short, not monotonic, or the caller armed no interval.
    #[must_use]
    pub fn growth_rate_per_sec(&self) -> Option<u64> {
        self.ring.growth_rate_kb_sec(self.sample_interval_ms)
    }

    /// Samples currently held in the window (`N` once it has wrapped).
    #[must_use]
    pub const fn samples(&self) -> usize {
        self.ring.len()
    }

    /// Newest count in the window; `None` before the first readable sample.
    #[must_use]
    pub const fn newest(&self) -> Option<u64> {
        self.ring.newest()
    }

    /// Configured trip mark, in handles (`0` = disabled).
    #[must_use]
    pub const fn trip(&self) -> u64 {
        self.trip
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// tests exercise the panic paths on purpose; the unwinds ARE the contract
mod tests {
    use super::*;

    #[test]
    fn a_flat_baseline_never_reads_as_a_leak() {
        let mut watch: HandleWatch<3> = HandleWatch::new(8, 1_000);
        let mut probe = || Some(120);
        assert_eq!(
            watch.sample(&mut probe).map(LeakClassification::code),
            Some("warming")
        );
        assert_eq!(
            watch.sample(&mut probe).map(LeakClassification::code),
            Some("warming")
        );
        assert_eq!(
            watch.sample(&mut probe).map(LeakClassification::code),
            Some("sawtooth"),
            "a full window of equal samples is not monotonic"
        );
        assert!(!watch.is_leaking());

        // A slow climb under the trip mark is Stable, not a leak.
        let mut calm: HandleWatch<3> = HandleWatch::new(8, 1_000);
        for count in [100u64, 101, 102] {
            calm.note(count);
        }
        assert_eq!(
            calm.note(103),
            LeakClassification::Stable { growth_kb: 2 },
            "two handles of drift over the last three samples"
        );
        assert!(!calm.is_leaking());
    }

    #[test]
    fn a_rising_window_names_the_leak_and_its_rate() {
        let mut watch: HandleWatch<4> = HandleWatch::new(8, 1_000);
        for count in [100u64, 104, 108, 112] {
            watch.note(count);
        }
        assert_eq!(
            watch.classification(),
            LeakClassification::Rising {
                growth_kb: 12,
                rate_kb_sec: Some(4),
            }
        );
        assert!(watch.is_leaking());
        assert_eq!(watch.growth_rate_per_sec(), Some(4));
        assert_eq!(watch.trip(), 8);

        // Trip 0 disables the verdict, not the window.
        let mut off: HandleWatch<3> = HandleWatch::new(0, 1_000);
        for count in [1u64, 2, 3] {
            off.note(count);
        }
        assert_eq!(
            off.classification(),
            LeakClassification::Stable { growth_kb: 2 }
        );
        assert!(!off.is_leaking());
    }

    #[test]
    fn an_unreadable_probe_records_nothing() {
        let mut watch: HandleWatch<3> = HandleWatch::new(8, 1_000);
        watch.note(100);
        watch.note(200);
        let mut blind = || None;
        assert_eq!(
            watch.sample(&mut blind),
            None,
            "a probe that cannot read says nothing"
        );
        assert_eq!(watch.samples(), 2, "and records no sample either");
        assert_eq!(
            watch.newest(),
            Some(200),
            "a zero would have been a false dip"
        );

        // Recovering the probe resumes on the same evidence.
        let mut back = || Some(300);
        assert_eq!(
            watch.sample(&mut back).map(LeakClassification::code),
            Some("rising")
        );
    }

    #[test]
    fn a_steep_partial_window_is_advisory_and_never_leaking() {
        let mut watch: HandleWatch<6> = HandleWatch::new(8, 1_000);
        for count in [100u64, 104, 108] {
            watch.note(count);
        }
        assert_eq!(
            watch.classification(),
            LeakClassification::Advisory {
                growth_kb: 8,
                projected_growth_kb: 20,
                samples: 3,
                rate_kb_sec: Some(4),
            },
            "+8 handles over two intervals, projected across five"
        );
        assert!(
            !watch.is_leaking(),
            "an advisory slope refuses nothing: the window is not full"
        );

        // Filling the window while the climb continues makes it a real leak.
        for count in [112u64, 116, 120] {
            watch.note(count);
        }
        assert_eq!(
            watch.classification(),
            LeakClassification::Rising {
                growth_kb: 20,
                rate_kb_sec: Some(4),
            }
        );
        assert!(watch.is_leaking());
    }

    #[test]
    fn a_dip_clears_the_handle_window_too() {
        let mut watch: HandleWatch<3> = HandleWatch::new(1, 1_000);
        watch.note(10);
        watch.note(20);
        assert!(watch.note(30).is_rising());
        assert!(
            !watch.note(5).is_rising(),
            "handles closed: the evidence is gone"
        );
        assert_eq!(watch.classification(), LeakClassification::Sawtooth);
    }
}
