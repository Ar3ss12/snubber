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
//! supervisor restart them, hard-only nodes keep admitting until the OS
//! kills them, both-zero nodes behave exactly as before (zero cost).

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

#[cfg(test)]
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
}
