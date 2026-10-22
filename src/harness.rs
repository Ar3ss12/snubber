//! Boundary harness: measure a corridor, then derive an envelope from it.
//!
//! [`ExecutionEnvelope`] needs numbers that come from the workload, not from
//! intuition. [`ProfileHarness`] runs one operation under a deterministic
//! [`FastPrng`] across a payload range and reports the matrix that matters:
//! **min / median / max**, **jitter** (max − min) and **bursts** (runs of
//! consecutive samples above the median).
//!
//! Determinism has two levels:
//! * the PRNG sequence depends only on the seed (xorshift32, no OS entropy);
//! * [`ProfileHarness::run_with_clock`] takes an injected nanosecond clock,
//!   so tests get exact microseconds instead of scheduler noise.
//!
//! [`HarnessReport::envelope`] turns the observed range into an
//! [`ExecutionEnvelope`] with headroom — measure first, promise second.

use crate::envelope::ExecutionEnvelope;
use core::time::Duration;

/// xorshift32: three shifts and three xors, no allocation, period 2^32 − 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastPrng(u32);

impl FastPrng {
    /// Seed the generator. `0` is swapped for the golden-ratio constant —
    /// xorshift32 is stuck at zero forever if it ever reaches it.
    #[must_use]
    pub const fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }

    /// Next raw 32-bit value.
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform-ish value in `0..bound` (modulo bias is irrelevant for
    /// synthetic payload sizes). `bound == 0` returns `0`.
    pub fn next_below(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            0
        } else {
            self.next_u32() % bound
        }
    }

    /// Current internal state (for reproducible replay).
    #[must_use]
    pub const fn state(&self) -> u32 {
        self.0
    }
}

/// What one boundary run observed. All durations are microseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessReport {
    /// Measured samples (warm-up excluded).
    pub samples: u32,
    /// Untimed warm-up invocations before sampling.
    pub warmup: u32,
    /// Seed that produced the payload sequence.
    pub seed: u32,
    /// Fastest sample.
    pub min_us: u64,
    /// Middle sample (`sorted[len / 2]`, upper median for even counts).
    pub median_us: u64,
    /// Slowest sample.
    pub max_us: u64,
    /// Spread: `max_us - min_us`.
    pub jitter_us: u64,
    /// Non-overlapping runs of `burst_len` consecutive above-median samples.
    pub bursts: u32,
    /// Samples that belonged to those runs.
    pub burst_samples: u32,
    /// Smallest payload handed to the workload (warm-up included), bytes.
    pub payload_min: u64,
    /// Largest payload handed to the workload (warm-up included), bytes.
    pub payload_max: u64,
    /// Sum of all measured samples, µs.
    pub total_us: u64,
}

impl HarnessReport {
    /// Arithmetic mean of the samples (`0` when there are none).
    #[must_use]
    pub fn mean_us(&self) -> u64 {
        if self.samples == 0 {
            0
        } else {
            self.total_us / u64::from(self.samples)
        }
    }

    /// Stable one-line report card (field order never changes).
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "samples={} min_us={} median_us={} max_us={} jitter_us={} bursts={} burst_samples={} payload={}..{}B seed={}",
            self.samples,
            self.min_us,
            self.median_us,
            self.max_us,
            self.jitter_us,
            self.bursts,
            self.burst_samples,
            self.payload_min,
            self.payload_max,
            self.seed
        )
    }

    /// Derive a corridor from the samples with `headroom_pct` of slack on
    /// both walls: the floor drops by that percentage, the ceiling rises by
    /// it. The payload range is kept exactly as observed. Durations above
    /// `u32::MAX` µs saturate — an envelope cannot promise more than 71 min.
    #[must_use]
    pub fn envelope(&self, headroom_pct: u8) -> ExecutionEnvelope {
        let h = u64::from(headroom_pct.min(100));
        let floor = self.min_us.saturating_mul(100 - h) / 100;
        let ceiling = self.max_us.saturating_mul(100 + h) / 100;
        ExecutionEnvelope::new(
            saturate_u32(floor),
            saturate_u32(ceiling),
            saturate_u32(self.payload_min),
            saturate_u32(self.payload_max),
        )
    }
}

fn saturate_u32(v: u64) -> u32 {
    if v > u64::from(u32::MAX) {
        u32::MAX
    } else {
        v as u32
    }
}

/// Sampling plan: how many measurements, how much warm-up, what counts as a
/// burst, and the synthetic payload range handed to the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileHarness {
    samples: u32,
    warmup: u32,
    burst_len: u32,
    payload_min: u32,
    payload_max: u32,
}

impl Default for ProfileHarness {
    fn default() -> Self {
        Self {
            samples: 64,
            warmup: 4,
            burst_len: 3,
            payload_min: 0,
            payload_max: 0,
        }
    }
}

impl ProfileHarness {
    /// Create a plan. `samples` is clamped to at least 1; `burst_len` to at
    /// least 2 (a burst of one is just a slow sample).
    #[must_use]
    pub const fn new(samples: u32, warmup: u32, burst_len: u32) -> Self {
        Self {
            samples: if samples == 0 { 1 } else { samples },
            warmup,
            burst_len: if burst_len < 2 { 2 } else { burst_len },
            payload_min: 0,
            payload_max: 0,
        }
    }

    /// Randomize the payload in `min..=max` (inclusive). A reversed range is
    /// swapped, so the corridor is never empty.
    #[must_use]
    pub const fn payload_range(mut self, min: u32, max: u32) -> Self {
        let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
        self.payload_min = lo;
        self.payload_max = hi;
        self
    }

    /// Measured sample count.
    #[must_use]
    pub const fn samples(&self) -> u32 {
        self.samples
    }

    /// Burst length in consecutive above-median samples.
    #[must_use]
    pub const fn burst_len(&self) -> u32 {
        self.burst_len
    }

    /// Run the workload on the process clock ([`std::time::Instant`]).
    pub fn run<F: FnMut(u64)>(&self, seed: u32, work: F) -> HarnessReport {
        self.run_with_clock(seed, std_now_ns, work)
    }

    /// Run the workload on an injected nanosecond clock (tests, Miri).
    ///
    /// Each sample reads the clock once before and once after `work`; the
    /// difference is truncated to microseconds. Warm-up invocations are
    /// never timed and consume payloads from the same sequence.
    pub fn run_with_clock<F: FnMut(u64)>(
        &self,
        seed: u32,
        wall: fn() -> u64,
        mut work: F,
    ) -> HarnessReport {
        let mut rng = FastPrng::new(seed);
        let payload = |rng: &mut FastPrng, lo: u32, hi: u32| -> u64 {
            if lo == hi {
                return u64::from(lo);
            }
            let span = hi - lo;
            if span == u32::MAX {
                u64::from(rng.next_u32())
            } else {
                u64::from(lo + rng.next_below(span + 1))
            }
        };

        let mut payload_min = u64::MAX;
        let mut payload_max = 0u64;
        for _ in 0..self.warmup {
            let bytes = payload(&mut rng, self.payload_min, self.payload_max);
            payload_min = payload_min.min(bytes);
            payload_max = payload_max.max(bytes);
            work(bytes);
        }

        let count = self.samples as usize;
        let mut samples = Vec::with_capacity(count);
        let mut total_us = 0u64;
        for _ in 0..count {
            let bytes = payload(&mut rng, self.payload_min, self.payload_max);
            payload_min = payload_min.min(bytes);
            payload_max = payload_max.max(bytes);
            let t0 = wall();
            work(bytes);
            let t1 = wall();
            let us = Duration::from_nanos(t1.saturating_sub(t0)).as_micros() as u64;
            total_us = total_us.saturating_add(us);
            samples.push(us);
        }

        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let min_us = sorted[0];
        let max_us = sorted[count - 1];
        let median_us = sorted[count / 2];
        let jitter_us = max_us.saturating_sub(min_us);

        let mut bursts = 0u32;
        let mut burst_samples = 0u32;
        let mut streak = 0u32;
        for &sample in &samples {
            if sample > median_us {
                streak += 1;
                if streak == self.burst_len {
                    bursts += 1;
                    burst_samples = burst_samples.saturating_add(streak);
                    streak = 0;
                }
            } else {
                streak = 0;
            }
        }

        HarnessReport {
            samples: self.samples,
            warmup: self.warmup,
            seed: FastPrng::new(seed).state(),
            min_us,
            median_us,
            max_us,
            jitter_us,
            bursts,
            burst_samples,
            payload_min,
            payload_max,
            total_us,
        }
    }
}

fn std_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static BOOT: OnceLock<Instant> = OnceLock::new();
    BOOT.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// One-shot harness over the defaults (64 samples, 4 warm-up, burst 3,
/// payload `0`), on the process clock. Returns the report card; print
/// [`HarnessReport::summary`] or derive [`HarnessReport::envelope`].
pub fn run_boundary_harness<F: FnMut(u64)>(samples: u32, seed: u32, work: F) -> HarnessReport {
    ProfileHarness::new(samples, 4, 3).run(seed, work)
}
