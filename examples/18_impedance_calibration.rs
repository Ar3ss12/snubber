//! Pattern 18: Impedance Calibration — measure, promise, judge.
//!
//! The envelope from the paper ("work must take 100–1000 µs") is a guess
//! until it meets the machine. This demo closes the loop:
//!
//! 1. [`ProfileHarness`] times the workload across a synthetic payload range
//!    and prints the min/median/max/jitter/burst matrix.
//! 2. [`HarnessReport::envelope`] derives an [`ExecutionEnvelope`] with
//!    headroom on both walls.
//! 3. A [`SnubberNode`] configured with that envelope judges real profiled
//!    settlements via `commit_enveloped` — including a deliberate no-op that
//!    must land **below the floor** and be counted, not tripped.
//!
//! Run with:
//! ```powershell
//! cargo run --example 18_impedance_calibration
//! ```

use snubber::{ProfileHarness, SnubberNode};
use std::hint::black_box;

const PAYLOAD_BYTES: u64 = 8_000;

fn workload(iterations: u64) -> u64 {
    let mut acc = 0x9E37_79B9_u64 ^ iterations;
    for _ in 0..iterations {
        acc = acc
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
    }
    black_box(acc)
}

fn main() {
    println!("=== Pattern 18: Impedance Calibration ===");

    let report = ProfileHarness::new(64, 4, 3)
        .payload_range(4_000, 12_000)
        .run(0x5EED, |bytes| {
            workload(bytes);
        });
    println!("[HARNESS] {}", report.summary());

    let envelope = report.envelope(50);
    println!(
        "[ENVELOPE] min_wall_us={} max_wall_us={} min_payload={} max_payload={}",
        envelope.min_wall_us, envelope.max_wall_us, envelope.min_payload, envelope.max_payload
    );

    let node = SnubberNode::builder("calibrated")
        .execution_envelope(envelope)
        .build();

    let mut inside = 0u32;
    let mut under = 0u32;
    let mut over = 0u32;
    for _ in 0..8 {
        let guard = node.enter_auto_profiled(|| {}).expect("admit");
        workload(PAYLOAD_BYTES);
        let (_event, _profile, verdict) = guard.commit_enveloped(PAYLOAD_BYTES);
        if verdict.is_inside() {
            inside += 1;
        } else if verdict.has_underflow() {
            under += 1;
        } else {
            over += 1;
        }
    }
    println!("[PROBE] inside={inside} under={under} over={over}");

    // A path that skipped its work: zero payload, no time on the clock.
    let guard = node.enter_auto_profiled(|| {}).expect("admit");
    let (_event, profile, verdict) = guard.commit_enveloped(0);
    println!(
        "[UNDERFLOW] work_wall_us={} time={:?} payload={:?}",
        profile.work_wall_us, verdict.time, verdict.payload
    );

    let stats = node.stats();
    println!(
        "[STATS] committed={} envelope_underflows={} envelope_overflows={} breaker={:?}",
        stats.committed, stats.envelope_underflows, stats.envelope_overflows, stats.breaker_state
    );
}
