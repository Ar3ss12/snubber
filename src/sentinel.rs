//! Liveness witness state machine (`core`-only).
//!
//! Layer 4 needs one decision language for "is the supervised process still
//! there?". The OS call that answers it (`OpenProcess`/`pidfd`/Job Object)
//! belongs to the application, exactly like `OrphanGuard`: the crate types
//! the promise and keeps the arithmetic. Nothing here reads a
//! clock, allocates, formats or touches a system handle — the caller feeds one
//! [`SentinelSample`] per observation and receives one [`SentinelVerdict`].
//!
//! `None` from a witness means "cannot observe", never "dead": a backend that
//! lacks a handle must not trigger a rescue of a live process.

/// Observer of an already running process.
pub trait ProcessWitness: Send + Sync {
    /// `Some(true)` = observed alive, `Some(false)` = observed dead,
    /// `None` = this backend cannot observe right now (never "dead").
    fn alive(&self) -> Option<bool>;

    /// Stable backend name for logs and post-mortem dumps.
    fn backend_name(&self) -> &'static str;
}

/// Witness with no observation channel at all: `alive()` is always `None`.
///
/// The honest default — a node wired without an OS adapter reports `Alive`
/// forever instead of inventing a `Dead` verdict.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWitness;

impl ProcessWitness for NoWitness {
    #[inline]
    fn alive(&self) -> Option<bool> {
        None
    }

    #[inline]
    fn backend_name(&self) -> &'static str {
        "none (cannot observe)"
    }
}

/// What one observation says about the watched process.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelVerdict {
    /// Witness alive or unobservable, and silence is inside the grace.
    Alive,
    /// Would still be alive, but the last token is older than the grace.
    Silent {
        /// Measured silence, unchanged from the sample.
        idle_ms: u64,
    },
    /// Witness observed the process gone. `armed` says whether a token was
    /// still outstanding — the hardware may be left energized.
    Dead {
        /// A token was outstanding at the time of the sample.
        armed: bool,
    },
}

/// Tuning for [`Sentinel`]. The field is private so a later knob is not a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentinelConfig {
    idle_grace_ms: u64,
}

impl SentinelConfig {
    /// `idle_grace_ms = 0` disables the `Silent` verdict (same law as
    /// `stall_ceiling_ms = 0`), leaving death detection only.
    #[must_use]
    pub const fn new(idle_grace_ms: u64) -> Self {
        Self { idle_grace_ms }
    }

    /// Grace applied to the sample's `idle_ms` before `Silent` is returned.
    #[must_use]
    pub const fn idle_grace_ms(&self) -> u64 {
        self.idle_grace_ms
    }
}

/// One observation: token silence, whether a token is outstanding, and what
/// the witness could see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentinelSample {
    /// Milliseconds since the last token arrived.
    pub idle_ms: u64,
    /// A token was armed at the time of the sample.
    pub armed: bool,
    /// `Some(true)` alive, `Some(false)` dead, `None` cannot observe.
    pub witness_alive: Option<bool>,
}

/// Pure rule: verdict from config and one sample. No clock, no state.
#[must_use]
pub fn classify_liveness(cfg: SentinelConfig, sample: SentinelSample) -> SentinelVerdict {
    if sample.witness_alive == Some(false) {
        return SentinelVerdict::Dead {
            armed: sample.armed,
        };
    }
    if cfg.idle_grace_ms > 0 && sample.idle_ms > cfg.idle_grace_ms {
        return SentinelVerdict::Silent {
            idle_ms: sample.idle_ms,
        };
    }
    SentinelVerdict::Alive
}

/// Stateful wrapper that remembers the last verdict.
#[derive(Debug, Clone, Copy)]
pub struct Sentinel {
    cfg: SentinelConfig,
    last: SentinelVerdict,
}

impl Sentinel {
    /// Start in `Alive`: nothing has been observed yet.
    #[must_use]
    pub const fn new(cfg: SentinelConfig) -> Self {
        Self {
            cfg,
            last: SentinelVerdict::Alive,
        }
    }

    /// Classify one sample and remember the verdict.
    pub fn observe(&mut self, sample: SentinelSample) -> SentinelVerdict {
        let verdict = classify_liveness(self.cfg, sample);
        self.last = verdict;
        verdict
    }

    /// Verdict from the most recent [`Self::observe`].
    #[must_use]
    pub const fn last(&self) -> SentinelVerdict {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(idle_ms: u64, armed: bool, witness_alive: Option<bool>) -> SentinelSample {
        SentinelSample {
            idle_ms,
            armed,
            witness_alive,
        }
    }

    #[test]
    fn death_wins_over_every_other_signal() {
        let cfg = SentinelConfig::new(1_000);
        assert_eq!(
            classify_liveness(cfg, sample(0, true, Some(false))),
            SentinelVerdict::Dead { armed: true },
            "a witness that saw the process die outranks a fresh token"
        );
        assert_eq!(
            classify_liveness(cfg, sample(5_000, false, Some(false))),
            SentinelVerdict::Dead { armed: false }
        );
    }

    #[test]
    fn unobservable_witness_never_reports_dead() {
        let cfg = SentinelConfig::new(10);
        assert_eq!(
            classify_liveness(cfg, sample(0, true, None)),
            SentinelVerdict::Alive,
            "None is `cannot observe`, not `dead`"
        );
        assert_eq!(
            classify_liveness(cfg, sample(50, true, None)),
            SentinelVerdict::Silent { idle_ms: 50 }
        );
    }

    #[test]
    fn zero_grace_disables_silent() {
        let cfg = SentinelConfig::new(0);
        assert_eq!(
            classify_liveness(cfg, sample(u64::MAX, true, Some(true))),
            SentinelVerdict::Alive,
            "grace 0 keeps death detection but never mints Silent"
        );
    }

    #[test]
    fn idle_equal_to_grace_is_still_alive() {
        let cfg = SentinelConfig::new(250);
        assert_eq!(
            classify_liveness(cfg, sample(250, true, Some(true))),
            SentinelVerdict::Alive
        );
        assert_eq!(
            classify_liveness(cfg, sample(251, true, Some(true))),
            SentinelVerdict::Silent { idle_ms: 251 }
        );
    }

    #[test]
    fn sentinel_remembers_the_last_verdict() {
        let mut sentinel = Sentinel::new(SentinelConfig::new(100));
        assert_eq!(sentinel.last(), SentinelVerdict::Alive);
        let seen = sentinel.observe(sample(500, true, Some(true)));
        assert_eq!(seen, SentinelVerdict::Silent { idle_ms: 500 });
        assert_eq!(sentinel.last(), seen);
        let seen = sentinel.observe(sample(1, false, Some(false)));
        assert_eq!(seen, SentinelVerdict::Dead { armed: false });
        assert_eq!(sentinel.last(), seen);
    }

    #[test]
    fn no_witness_says_so() {
        let witness = NoWitness;
        assert_eq!(witness.alive(), None);
        assert_eq!(witness.backend_name(), "none (cannot observe)");
    }
}
