//! Execution envelope: the corridor a guarded transaction promised to stay in.
//!
//! A budget answers "did it cost too much?". An envelope also answers the
//! other half: "did it do too little?". A 40 ms task that returns in 200 µs
//! did not get faster — it skipped work (early return, stale cache, a loop
//! that never ran). Money and telemetry code needs both bounds.
//!
//! [`ExecutionEnvelope`] is 16 bytes, `Copy`, and lives on the node (builder
//! [`SnubberNodeBuilder::execution_envelope`](crate::SnubberNodeBuilder::execution_envelope)).
//! `0` disables an individual bound; zeroing all four disables the envelope
//! entirely. Verdicts are computed once at settlement from the
//! [`ExecutionProfile`](crate::ExecutionProfile) and the payload byte count —
//! no clock reads, no allocation, no breaker feed.

/// Time half of the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TimeVerdict {
    /// Inside the declared corridor (or no bounds declared).
    #[default]
    Inside,
    /// Work finished faster than the floor declared. Not a failure: a
    /// signal that the measured path did less than promised.
    Under {
        /// Observed wall time, µs.
        wall_us: u64,
        /// Declared floor, µs.
        min_us: u64,
    },
    /// Work exceeded the ceiling.
    Over {
        /// Observed wall time, µs.
        wall_us: u64,
        /// Declared ceiling, µs.
        max_us: u64,
    },
}

/// Payload half of the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PayloadVerdict {
    /// Inside the declared corridor (or no bounds declared).
    #[default]
    Inside,
    /// Payload smaller than the floor. Same non-catastrophic signal as
    /// [`TimeVerdict::Under`].
    Under {
        /// Observed payload size, bytes.
        bytes: u64,
        /// Declared floor, bytes.
        min: u64,
    },
    /// Payload exceeded the ceiling.
    Over {
        /// Observed payload size, bytes.
        bytes: u64,
        /// Declared ceiling, bytes.
        max: u64,
    },
}

/// Both halves of one settlement, computed together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EnvelopeVerdict {
    /// Wall-time corridor result.
    pub time: TimeVerdict,
    /// Payload corridor result.
    pub payload: PayloadVerdict,
}

impl EnvelopeVerdict {
    /// Both halves inside (also the verdict for a disabled envelope).
    #[must_use]
    pub const fn inside() -> Self {
        Self {
            time: TimeVerdict::Inside,
            payload: PayloadVerdict::Inside,
        }
    }

    /// No bound was crossed.
    #[must_use]
    pub const fn is_inside(&self) -> bool {
        matches!(self.time, TimeVerdict::Inside) && matches!(self.payload, PayloadVerdict::Inside)
    }

    /// At least one floor was not reached.
    #[must_use]
    pub const fn has_underflow(&self) -> bool {
        matches!(self.time, TimeVerdict::Under { .. })
            || matches!(self.payload, PayloadVerdict::Under { .. })
    }

    /// At least one ceiling was exceeded.
    #[must_use]
    pub const fn has_overflow(&self) -> bool {
        matches!(self.time, TimeVerdict::Over { .. })
            || matches!(self.payload, PayloadVerdict::Over { .. })
    }
}

/// Declared corridor for one guarded transaction: wall µs and payload bytes.
///
/// Layout (16 B, `#[repr(C)]`): `min_wall_us | max_wall_us | min_payload |
/// max_payload`, each `u32`. `u32` microseconds cap at ≈71 min and `u32`
/// payload bytes at 4 GiB — both beyond any corridor that is useful to
/// declare. A declared floor above its ceiling clamps the ceiling up to the
/// floor (`new` keeps the invariant `max == 0 || max >= min`), so a corridor
/// never becomes empty by accident.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecutionEnvelope {
    /// Lower wall bound, µs (`0` = no floor).
    pub min_wall_us: u32,
    /// Upper wall bound, µs (`0` = no ceiling).
    pub max_wall_us: u32,
    /// Lower payload bound, bytes (`0` = no floor).
    pub min_payload: u32,
    /// Upper payload bound, bytes (`0` = no ceiling).
    pub max_payload: u32,
}

// COMPILE-TIME GATE: the envelope is a 16-byte Copy header, not storage.
const _: () = assert!(core::mem::size_of::<ExecutionEnvelope>() == 16);

impl ExecutionEnvelope {
    /// Disabled envelope: all four bounds off.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            min_wall_us: 0,
            max_wall_us: 0,
            min_payload: 0,
            max_payload: 0,
        }
    }

    /// Build a corridor. A non-zero ceiling below its floor is raised to
    /// the floor (the floor is the stronger promise).
    #[must_use]
    pub const fn new(
        min_wall_us: u32,
        max_wall_us: u32,
        min_payload: u32,
        max_payload: u32,
    ) -> Self {
        let max_wall_us = if max_wall_us != 0 && max_wall_us < min_wall_us {
            min_wall_us
        } else {
            max_wall_us
        };
        let max_payload = if max_payload != 0 && max_payload < min_payload {
            min_payload
        } else {
            max_payload
        };
        Self {
            min_wall_us,
            max_wall_us,
            min_payload,
            max_payload,
        }
    }

    /// Every bound is `0` — nothing to judge.
    #[must_use]
    pub const fn is_off(&self) -> bool {
        self.min_wall_us == 0
            && self.max_wall_us == 0
            && self.min_payload == 0
            && self.max_payload == 0
    }

    /// Judge the wall-time half. Bounds are inclusive: `wall_us == min`
    /// and `wall_us == max` are inside.
    #[must_use]
    pub const fn time_verdict(&self, wall_us: u64) -> TimeVerdict {
        if self.min_wall_us != 0 && wall_us < self.min_wall_us as u64 {
            return TimeVerdict::Under {
                wall_us,
                min_us: self.min_wall_us as u64,
            };
        }
        if self.max_wall_us != 0 && wall_us > self.max_wall_us as u64 {
            return TimeVerdict::Over {
                wall_us,
                max_us: self.max_wall_us as u64,
            };
        }
        TimeVerdict::Inside
    }

    /// Judge the payload half. Bounds are inclusive.
    #[must_use]
    pub const fn payload_verdict(&self, bytes: u64) -> PayloadVerdict {
        if self.min_payload != 0 && bytes < self.min_payload as u64 {
            return PayloadVerdict::Under {
                bytes,
                min: self.min_payload as u64,
            };
        }
        if self.max_payload != 0 && bytes > self.max_payload as u64 {
            return PayloadVerdict::Over {
                bytes,
                max: self.max_payload as u64,
            };
        }
        PayloadVerdict::Inside
    }

    /// Wall time raised to the declared floor (`0` floor = unchanged).
    ///
    /// This is for the caller that must not finish faster than its declared
    /// floor — a pacer, a coalescing window — and pads its own WAIT with the
    /// result. It is not a way to make a skipped unit of work look like a
    /// completed one: padding the measurement instead of the work is exactly
    /// the signal the floor exists to catch, so apply it to the wait, never
    /// to the reported duration of work that did not happen.
    ///
    /// ```
    /// use snubber::ExecutionEnvelope;
    ///
    /// let env = ExecutionEnvelope::new(500, 2_000, 0, 0);
    /// assert_eq!(env.pad_to_floor(120), 500);   // floored up
    /// assert_eq!(env.pad_to_floor(900), 900);   // already above, untouched
    /// ```
    #[must_use]
    pub const fn pad_to_floor(&self, wall_us: u64) -> u64 {
        if wall_us < self.min_wall_us as u64 {
            self.min_wall_us as u64
        } else {
            wall_us
        }
    }

    /// Payload size raised to the declared floor (`0` floor = unchanged).
    /// The same rule as [`pad_to_floor`](Self::pad_to_floor), applied to the
    /// payload half of the corridor.
    #[must_use]
    pub const fn pad_payload_to_floor(&self, bytes: u64) -> u64 {
        if bytes < self.min_payload as u64 {
            self.min_payload as u64
        } else {
            bytes
        }
    }

    /// Judge both halves in one call.
    #[must_use]
    pub const fn judge(&self, wall_us: u64, payload_bytes: u64) -> EnvelopeVerdict {
        EnvelopeVerdict {
            time: self.time_verdict(wall_us),
            payload: self.payload_verdict(payload_bytes),
        }
    }
}
