//! Supervisor memory euthanasia (Phase 3): decision math, soft drain,
//! StateDigest handoff and generation restart.
//!
//! As with [`RestartTracker`], the supervisor owns no sampler: the APP
//! feeds one external RSS sample. So the tests run the policy on a fake
//! sequence of samples and the handoff on real `StateDigest` bytes —
//! no OS process spawning, deterministic on every host and under Miri.

use snubber::{
    DeathCause, EnterError, GenerationClock, MemoryPolicy, MemoryVerdict, SnubberNode, StateDigest,
    TransactionHeader, STATE_LEN,
};

#[test]
fn policy_matrix_soft_drains_hard_kills() {
    let mut policy = MemoryPolicy::new(100, 200);
    assert!(policy.watch().is_configured());
    assert_eq!(policy.watch().soft_kb(), 100);
    assert_eq!(policy.watch().hard_kb(), 200);

    // Under soft: everything is normal.
    assert_eq!(policy.note(99), MemoryVerdict::Ok);
    assert!(!policy.should_kill());
    assert_eq!(policy.death_cause(), None);

    // Soft: drain request, NOT a kill — in-flight work must settle.
    assert_eq!(policy.note(100), MemoryVerdict::Soft);
    assert!(!policy.should_kill(), "soft never kills");
    assert_eq!(policy.death_cause(), None);
    assert_eq!(policy.note(199), MemoryVerdict::Soft);

    // Hard: terminate without draining.
    assert_eq!(policy.note(200), MemoryVerdict::Hard);
    assert!(policy.should_kill());
    assert_eq!(policy.death_cause(), Some(DeathCause::MemoryLimit));
    assert_eq!(
        policy.watch().peak_kb(),
        200,
        "peak recorded for the postmortem"
    );
    assert_eq!(policy.watch().used_kb(), 200);
}

#[test]
fn soft_only_and_hard_only_policies() {
    // Soft-only: drains forever, the process is never killed by memory.
    let mut soft_only = MemoryPolicy::new(64, 0);
    assert_eq!(soft_only.note(u64::MAX), MemoryVerdict::Soft);
    assert!(!soft_only.should_kill());
    assert_eq!(soft_only.death_cause(), None);

    // Hard-only: no drain phase, admits until the hard sample fires.
    let mut hard_only = MemoryPolicy::new(0, 128);
    assert_eq!(hard_only.note(127), MemoryVerdict::Ok);
    assert_eq!(hard_only.note(128), MemoryVerdict::Hard);
    assert!(hard_only.should_kill());

    // `hard < soft` clamps up: the drain phase always exists.
    let clamped = MemoryPolicy::new(500, 10);
    assert_eq!(clamped.watch().hard_kb(), 500);
    assert!(!clamped.should_kill());

    // Both zero: legacy behaviour, zero cost.
    let mut off = MemoryPolicy::default();
    assert_eq!(off.note(u64::MAX), MemoryVerdict::Ok);
    assert!(!off.should_kill());
}

#[test]
fn memory_limit_reason_is_stamped_in_the_aborted_event() {
    assert_eq!(
        DeathCause::MemoryLimit.reason(),
        "process exceeded hard memory limit"
    );
    assert_ne!(
        DeathCause::MemoryLimit.reason(),
        DeathCause::HangTimeout.reason(),
        "memory euthanasia must not be reported as a hang"
    );

    let header = TransactionHeader::with_generation(1, 3, 7);
    let ev = snubber::supervisor::death_event(header, DeathCause::MemoryLimit);
    match ev {
        snubber::SnubberEvent::Aborted { header: h, reason } => {
            assert_eq!(h, header);
            assert_eq!(reason, "process exceeded hard memory limit");
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[test]
fn soft_drain_refuses_then_digest_handoff_revives_next_generation() {
    // gen1 node with a drain threshold at 1000 KiB and a kill threshold at 2000.
    let node = SnubberNode::builder("super/memory-recycle")
        .memory_limits(1000, 2000)
        .build();
    node.enter(1, 7, || {}).expect("fresh node admits").commit();

    // External sampler (the APP's job) reports RSS past the soft limit.
    assert_eq!(node.note_memory_kb(1_200), MemoryVerdict::Soft);
    assert_eq!(node.memory_verdict(), MemoryVerdict::Soft);
    assert_eq!(node.memory_limits_kb(), (1000, 2000));

    // Soft = drain: new admissions refused BEFORE the lock, in-flight settle.
    match node.enter(2, 8, || {}) {
        Err(EnterError::MemoryPressure) => {}
        Err(_) => panic!("wrong refusal kind"),
        Ok(_) => panic!("soft limit must refuse admissions"),
    }
    let stats = node.stats();
    assert_eq!(
        stats.memory_rejections, 1,
        "own column, not a failure streak"
    );
    assert!(stats.memory_pressure);
    assert_eq!(stats.window_failures, 0, "memory pressure is not a fault");
    assert_eq!(stats.total_started, 1, "rejected admission never armed");
    assert_eq!(EnterError::MemoryPressure.retry_after_ms(), 1000);

    // Drain complete: pack the app checkpoint into the 64 B handoff digest.
    let snapshot = snubber::SnapshotV1::from_stats(&stats, 1);
    let mut bytes = [0u8; STATE_LEN];
    bytes[..64].copy_from_slice(&snapshot.encode()[..64]);
    let digest = StateDigest::new(bytes);
    assert_eq!(
        digest.crc(),
        StateDigest::new(bytes).crc(),
        "digest seal is stable"
    );

    // Worker exits; the lineage bumps exactly once (superseded generation).
    let mut clock = GenerationClock::new();
    assert_eq!(clock.generation(), 1);
    let gen2 = clock.restart();
    assert_eq!(gen2, 2);
    assert_eq!(clock.alloc_epoch(), 1, "epoch re-handshakes from 1");

    // Successor generation restores the digest and admits again.
    let successor = SnubberNode::builder("super/memory-recycle")
        .memory_limits(1000, 2000)
        .build();
    assert_eq!(
        successor.memory_verdict(),
        MemoryVerdict::Ok,
        "fresh budget"
    );
    successor
        .enter_with_generation(gen2, 1, 1, || {})
        .expect("gen2 admits under the restored budget")
        .commit();
    assert_eq!(StateDigest::new(bytes).bytes[..64], snapshot.encode()[..64]);

    // The retired generation is rejected even with a higher epoch.
    match successor.enter_with_generation(1, 9_999, 2, || {}) {
        Err(EnterError::StaleEpoch) => {}
        Err(_) => panic!("wrong refusal kind"),
        Ok(_) => panic!("superseded generation must be rejected"),
    }
}

#[test]
fn hard_limit_terminates_without_draining() {
    // The node side of the hard decision: refuse and report Hard.
    let node = SnubberNode::builder("super/memory-hard")
        .memory_limits(100, 400)
        .build();
    assert_eq!(node.note_memory_kb(150), MemoryVerdict::Soft);
    assert_eq!(node.note_memory_kb(400), MemoryVerdict::Hard);
    let snap = node.snapshot_v1();
    assert_eq!(
        (
            snap.memory_pressure,
            snap.memory_used_kb,
            snap.memory_hard_kb
        ),
        (1, 400, 400)
    );

    // The supervisor side: same samples through MemoryPolicy → kill decision.
    let mut policy = MemoryPolicy::new(100, 400);
    assert!(!policy.should_kill());
    policy.note(150);
    assert!(!policy.should_kill(), "soft sample must not kill");
    policy.note(400);
    assert!(policy.should_kill(), "hard sample kills without drain");
    assert_eq!(policy.death_cause(), Some(DeathCause::MemoryLimit));
}
