//! Preventive rotation: a bounded soft window, then a generation hand-off.
//!
//! The decision half runs on a fake clock — no process, no sampler, no sleep.
//! The hand-off half runs a real successor process: this test binary
//! re-executes itself with a piped stdin and reports what
//! `take_stdin_digest` saw there, so `spawn_successor` is pinned on the same
//! contract the supervisor relies on (64 B, then EOF — or immediate EOF).

use snubber::{
    GenerationClock, MemoryVerdict, PipeDrain, Rotation, RotationHandoff, RotationPolicy,
    RotationStep, StateDigest, Supervisor, STATE_LEN,
};
use std::path::PathBuf;
use std::process::Command;

const CHILD_ENV: &str = "SNUB_ROT_CHILD";
const OUT_ENV: &str = "SNUB_ROT_OUT";

fn digest_with(mark: u8) -> StateDigest {
    let mut bytes = [0u8; STATE_LEN];
    bytes[0] = mark;
    StateDigest::new(bytes)
}

fn in_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

#[test]
fn soft_opens_a_bounded_window_and_the_deadline_rotates() {
    if in_child() {
        return;
    }
    let mut rotation = Rotation::new(RotationPolicy::new(250, 1));

    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 1_000),
        RotationStep::Draining {
            soft_at_ms: 1_000,
            deadline_ms: 1_250
        }
    );
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 1_249),
        RotationStep::Draining {
            soft_at_ms: 1_000,
            deadline_ms: 1_250
        }
    );
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 1_250),
        RotationStep::Rotate {
            soft_at_ms: 1_000,
            waited_ms: 250
        }
    );
    // The order stands until the caller records the new generation: a sample
    // one tick later must not fall back to "still draining".
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 1_300),
        RotationStep::Rotate {
            soft_at_ms: 1_000,
            waited_ms: 300
        }
    );
    assert_eq!(rotation.rotations(), 0, "the caller has not rotated yet");
}

#[test]
fn a_sample_below_soft_disarms_the_window() {
    if in_child() {
        return;
    }
    let mut rotation = Rotation::new(RotationPolicy::new(50, 1));
    assert!(matches!(
        rotation.observe(MemoryVerdict::Soft, 100),
        RotationStep::Draining { .. }
    ));
    assert_eq!(
        rotation.observe(MemoryVerdict::Ok, 120),
        RotationStep::Watch,
        "a spike that recovers never rotates"
    );
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 300),
        RotationStep::Draining {
            soft_at_ms: 300,
            deadline_ms: 350
        }
    );
}

#[test]
fn hard_is_the_kill_path_not_the_rotation() {
    if in_child() {
        return;
    }
    let mut rotation = Rotation::new(RotationPolicy::new(100, 4));
    assert!(matches!(
        rotation.observe(MemoryVerdict::Soft, 0),
        RotationStep::Draining { .. }
    ));
    assert_eq!(
        rotation.observe(MemoryVerdict::Hard, 40),
        RotationStep::Watch,
        "over budget is MemoryPolicy.should_kill's decision"
    );
    assert_eq!(rotation.rotations(), 0);
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 500),
        RotationStep::Draining {
            soft_at_ms: 500,
            deadline_ms: 600
        },
        "the hard sample closed the window"
    );
}

#[test]
fn the_deadline_is_fixed_at_the_first_soft_sample() {
    if in_child() {
        return;
    }
    let mut rotation = Rotation::new(RotationPolicy::new(1_000, 1));
    assert!(matches!(
        rotation.observe(MemoryVerdict::Soft, 100),
        RotationStep::Draining {
            deadline_ms: 1_100,
            ..
        }
    ));
    assert!(
        matches!(
            rotation.observe(MemoryVerdict::Soft, 500),
            RotationStep::Draining {
                deadline_ms: 1_100,
                ..
            }
        ),
        "later soft samples must not push the deadline back"
    );
    assert!(matches!(
        rotation.observe(MemoryVerdict::Soft, 1_099),
        RotationStep::Draining { .. }
    ));
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 1_100),
        RotationStep::Rotate {
            soft_at_ms: 100,
            waited_ms: 1_000
        }
    );
}

#[test]
fn the_budget_bounds_the_rotations_then_soft_is_telemetry_again() {
    if in_child() {
        return;
    }
    let drain = PipeDrain::manual();
    let mut clock = GenerationClock::new();
    let mut rotation = Rotation::new(RotationPolicy::new(100, 2));

    for expected_gen in [2u64, 3] {
        assert!(matches!(
            rotation.observe(MemoryVerdict::Soft, expected_gen * 1_000),
            RotationStep::Draining { .. }
        ));
        assert!(matches!(
            rotation.observe(MemoryVerdict::Soft, expected_gen * 1_000 + 100),
            RotationStep::Rotate { .. }
        ));
        let handoff = rotation.handoff(&drain, &mut clock);
        assert_eq!(handoff.generation, expected_gen);
        assert_eq!(u64::from(handoff.rotations), expected_gen - 1);
    }

    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 9_000),
        RotationStep::Exhausted,
        "budget spent: soft is logged, the hard wall is the only stop"
    );
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 9_500),
        RotationStep::Exhausted
    );
    assert_eq!(rotation.rotations(), 2, "an exhausted window never rotates");
}

#[test]
fn a_zero_budget_is_unlimited() {
    if in_child() {
        return;
    }
    let drain = PipeDrain::manual();
    let mut clock = GenerationClock::new();
    let mut rotation = Rotation::new(RotationPolicy::new(10, 0));
    for gen in 2u64..7 {
        assert!(matches!(
            rotation.observe(MemoryVerdict::Soft, 0),
            RotationStep::Draining { .. }
        ));
        assert!(matches!(
            rotation.observe(MemoryVerdict::Soft, 10),
            RotationStep::Rotate { .. }
        ));
        assert_eq!(rotation.handoff(&drain, &mut clock).generation, gen);
    }
    assert_eq!(rotation.rotations(), 5);
}

#[test]
fn a_disabled_policy_never_rotates() {
    if in_child() {
        return;
    }
    assert!(RotationPolicy::default().is_off());
    assert!(RotationPolicy::new(0, 4).is_off(), "no window, no rotation");

    for policy in [RotationPolicy::default(), RotationPolicy::new(0, 4)] {
        let mut rotation = Rotation::new(policy);
        assert_eq!(
            rotation.observe(MemoryVerdict::Soft, 0),
            RotationStep::Watch
        );
        assert_eq!(
            rotation.observe(MemoryVerdict::Soft, u64::MAX),
            RotationStep::Watch
        );
        assert_eq!(rotation.rotations(), 0);
    }
    assert_eq!(
        Rotation::new(RotationPolicy::new(5, 1)).policy().grace_ms,
        5
    );
}

#[test]
fn restart_keeps_the_budget_without_counting_a_rotation() {
    if in_child() {
        return;
    }
    let mut rotation = Rotation::new(RotationPolicy::new(100, 1));
    assert!(matches!(
        rotation.observe(MemoryVerdict::Soft, 0),
        RotationStep::Draining { .. }
    ));
    rotation.restart(); // the child died on its own path
    assert_eq!(rotation.rotations(), 0);
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 200),
        RotationStep::Draining {
            soft_at_ms: 200,
            deadline_ms: 300
        },
        "a fresh child gets a fresh window"
    );
    assert!(
        !matches!(
            rotation.observe(MemoryVerdict::Soft, 300),
            RotationStep::Exhausted
        ),
        "an unplanned death never spends the rotation budget"
    );
}

#[test]
fn handoff_takes_the_last_verified_digest_and_retires_the_generation() {
    if in_child() {
        return;
    }
    let drain = PipeDrain::manual();
    for frame in digest_with(42).to_frames() {
        assert_eq!(drain.feed(&frame.to_bytes()), 1);
    }

    let mut rotation = Rotation::new(RotationPolicy::new(100, 1));
    assert!(matches!(
        rotation.observe(MemoryVerdict::Soft, 0),
        RotationStep::Draining { .. }
    ));
    assert!(matches!(
        rotation.observe(MemoryVerdict::Soft, 100),
        RotationStep::Rotate { .. }
    ));

    let mut clock = GenerationClock::new();
    let handoff = rotation.handoff(&drain, &mut clock);

    assert_eq!(handoff.digest.map(|d| d.bytes[0]), Some(42));
    assert!(handoff.has_digest());
    assert_eq!(handoff.generation, 2);
    assert_eq!(clock.generation(), 2, "the lineage moved with the handoff");
    assert_eq!(clock.alloc_epoch(), 1, "the successor re-handshakes from 1");
    assert_eq!(handoff.rotations, 1);
    assert_eq!(rotation.rotations(), 1);
    assert!(
        drain.take_state().is_none(),
        "the digest belongs to the successor and moves exactly once"
    );
    assert_eq!(
        rotation.observe(MemoryVerdict::Soft, 200),
        RotationStep::Exhausted,
        "the handoff consumed the only rotation"
    );
}

#[test]
fn a_torn_digest_restores_nothing_but_still_retires_the_generation() {
    if in_child() {
        return;
    }
    let drain = PipeDrain::manual();
    let frames = digest_with(7).to_frames();
    assert_eq!(drain.feed(&frames[0].to_bytes()), 1);
    assert_eq!(drain.feed(&frames[1].to_bytes()), 1);

    let mut rotation = Rotation::new(RotationPolicy::new(100, 1));
    let mut clock = GenerationClock::new();
    let handoff = rotation.handoff(&drain, &mut clock);

    assert!(
        handoff.digest.is_none(),
        "two chunks are not a restorable state"
    );
    assert!(!handoff.has_digest());
    assert_eq!(handoff.generation, 2, "a stale worker still owns nothing");
    assert_eq!(handoff.rotations, 1);
}

#[test]
fn a_successor_spawned_with_a_handoff_reads_the_digest_on_stdin() {
    if in_child() {
        return;
    }
    let out = report_path("some");
    let digest = digest_with(42);

    let mut cmd = Command::new(std::env::current_exe().expect("test exe"));
    cmd.env(CHILD_ENV, "1").env(OUT_ENV, &out);
    let handoff = RotationHandoff {
        digest: Some(digest),
        generation: 2,
        rotations: 1,
    };
    let mut child = Supervisor::spawn_successor(cmd, &handoff).expect("spawn successor");
    let status = child.wait().expect("successor exit");
    assert!(status.success(), "child test reported");

    let seen = std::fs::read_to_string(&out).expect("child report");
    let _ = std::fs::remove_file(&out);
    assert_eq!(
        seen.trim(),
        format!("SOME {:08X}", digest.crc()),
        "64 B crossed stdin bit-exact"
    );
}

#[test]
fn a_successor_without_a_digest_reads_eof_instead_of_blocking() {
    if in_child() {
        return;
    }
    let out = report_path("none");
    let mut cmd = Command::new(std::env::current_exe().expect("test exe"));
    cmd.env(CHILD_ENV, "1").env(OUT_ENV, &out);
    let handoff = RotationHandoff {
        digest: None,
        generation: 3,
        rotations: 2,
    };
    let mut child = Supervisor::spawn_successor(cmd, &handoff).expect("spawn successor");
    let status = child.wait().expect("successor exit");
    assert!(status.success(), "child test reported");

    let seen = std::fs::read_to_string(&out).expect("child report");
    let _ = std::fs::remove_file(&out);
    assert_eq!(
        seen.trim(),
        "NONE",
        "no digest means immediate EOF, never a hang"
    );
}

/// Child side: report what `take_stdin_digest` saw, then let the harness exit.
#[test]
#[cfg_attr(
    miri,
    ignore = "Miri does not support spawning OS processes or a real stdin"
)]
fn child_reports_the_handoff() {
    if !in_child() {
        return;
    }
    let report = match snubber::take_stdin_digest() {
        Some(digest) => format!("SOME {:08X}", digest.crc()),
        None => "NONE".to_string(),
    };
    let out = std::env::var_os(OUT_ENV).expect("child needs a report path");
    std::fs::write(out, report).expect("write report");
}

fn report_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("snub_rotation_{tag}_{}.txt", std::process::id()))
}
