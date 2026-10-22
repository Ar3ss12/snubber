//! Harness contract: deterministic PRNG, exact min/median/max/jitter/burst
//! accounting on an injected clock, and an envelope derived from the samples.

use snubber::{run_boundary_harness, ExecutionEnvelope, FastPrng, ProfileHarness};
use std::sync::atomic::{AtomicU64, Ordering};

/// Quadratic call counter: sample `k` sees `(4k + 3) * 10 µs`.
static CALLS: AtomicU64 = AtomicU64::new(0);

fn quadratic_clock() -> u64 {
    let i = CALLS.fetch_add(1, Ordering::SeqCst) + 1;
    i * i * 10_000
}

#[test]
fn exact_matrix_on_a_scripted_clock() {
    CALLS.store(0, Ordering::SeqCst);
    let report = ProfileHarness::new(8, 0, 3).run_with_clock(7, quadratic_clock, |_bytes| {});

    assert_eq!(report.samples, 8);
    assert_eq!(report.warmup, 0);
    assert_eq!(report.min_us, 30);
    assert_eq!(report.median_us, 190);
    assert_eq!(report.max_us, 310);
    assert_eq!(report.jitter_us, 280);
    assert_eq!(report.mean_us(), 170);
    assert_eq!(
        (report.bursts, report.burst_samples),
        (1, 3),
        "the last three samples (230/270/310) form one burst above the median"
    );
    assert_eq!(report.payload_min, 0);
    assert_eq!(report.payload_max, 0);

    let envelope = report.envelope(10);
    assert_eq!(
        envelope,
        ExecutionEnvelope::new(27, 341, 0, 0),
        "floor -10%, ceiling +10%"
    );
}

/// The workload paints its own durations: [1,1,1,10,1,1,10,10,10] µs.
static CLOCK: AtomicU64 = AtomicU64::new(0);

fn painted_clock() -> u64 {
    CLOCK.load(Ordering::SeqCst)
}

#[test]
fn bursts_are_non_overlapping_runs_above_the_median() {
    CLOCK.store(0, Ordering::SeqCst);
    let durations = [1u64, 1, 1, 10, 1, 1, 10, 10, 10];
    let mut next = 0usize;

    let report = ProfileHarness::new(9, 0, 3).run_with_clock(42, painted_clock, |_bytes| {
        let d = durations[next];
        next += 1;
        CLOCK.fetch_add(d * 1_000, Ordering::SeqCst);
    });

    assert_eq!(report.min_us, 1);
    assert_eq!(report.median_us, 1);
    assert_eq!(report.max_us, 10);
    assert_eq!(report.jitter_us, 9);
    assert_eq!(report.mean_us(), 5);
    assert_eq!(
        (report.bursts, report.burst_samples),
        (1, 3),
        "the lone slow sample at index 3 is shorter than burst_len"
    );
}

#[test]
fn prng_is_reproducible_and_never_sticks_at_zero() {
    let mut a = FastPrng::new(7);
    let mut b = FastPrng::new(7);
    for _ in 0..1_000 {
        assert_eq!(a.next_u32(), b.next_u32());
    }
    assert_ne!(a.state(), 0);
    assert_ne!(FastPrng::new(0).state(), 0, "zero seed is remapped");
    assert_eq!(FastPrng::new(1).next_below(0), 0);

    let mut rng = FastPrng::new(0);
    for _ in 0..1_000 {
        assert!(rng.next_below(10) < 10);
    }
}

#[test]
fn payload_range_is_respected_and_reported() {
    let mut observed_min = u64::MAX;
    let mut observed_max = 0u64;

    let report = ProfileHarness::new(64, 4, 3)
        .payload_range(64, 16)
        .run(9, |bytes| {
            observed_min = observed_min.min(bytes);
            observed_max = observed_max.max(bytes);
        });

    assert!(observed_min >= 16, "swapped range still clamps the floor");
    assert!(observed_max <= 64);
    assert_eq!(report.payload_min, observed_min);
    assert_eq!(report.payload_max, observed_max);
    assert_eq!(report.warmup, 4);
}

#[test]
fn run_boundary_harness_reports_sane_bounds() {
    let report = run_boundary_harness(16, 3, |_bytes| {
        std::hint::black_box(0u64);
    });

    assert_eq!(report.samples, 16);
    assert!(report.min_us <= report.median_us);
    assert!(report.median_us <= report.max_us);
    assert_eq!(report.jitter_us, report.max_us - report.min_us);
    let card = report.summary();
    assert!(card.starts_with("samples=16 "));
    assert!(card.contains("median_us="));
    assert!(card.contains("jitter_us="));
}
