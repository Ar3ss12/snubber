//! Watchdog chain: rotten hooks must not behead the chain.
//! New module `src/watchdog.rs` + contract `src/contract.rs` under test.

use snubber::{FaultAction, FaultKind, FaultPolicy, LogLevel, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn rotten_first_hook_does_not_silence_second() {
    let seen = Arc::new(AtomicU64::new(0));
    let probe = Arc::clone(&seen);
    let node = SnubberNode::builder("watch/chain")
        .on_fault(move |_, _, _| {
            probe.fetch_add(1, Ordering::SeqCst);
        })
        .on_fault(|_, _, _| panic!("telemetry plugin died"))
        .build();
    // NOTE: hooks run in registration order — rotten FIRST to prove
    // the second still fires. (If order ever changes, this test catches
    // the beheading the other way round.)
    let seen2 = Arc::clone(&seen);
    let node2 = SnubberNode::builder("watch/chain2")
        .on_fault(|_, _, _| panic!("rotten first"))
        .on_fault(move |_, _, _| {
            seen2.fetch_add(10, Ordering::SeqCst);
        })
        .build();
    let _ = node.enter(1, 1, || {});
    node.report(FaultKind::Panic, 2, 2);
    node2.report(FaultKind::Panic, 3, 3);
    // 1 (live hook on node) + 1 (implicit-drop hook on the uncommitted
    // enter above) + 10 (live hook after the rotten one on node2) = 12.
    // The rotten hooks printed panic noise to stderr (штатний hook, the
    // panic was caught) but beheaded NOBODY.
    assert_eq!(seen.load(Ordering::SeqCst), 12, "no hook beheaded");
    assert!(node2.stats().reported[FaultKind::Panic.index()] >= 1);
}

#[test]
fn fallen_doctor_does_not_cancel_diagnosis() {
    fn dying_reset() {
        panic!("emergency reset exploded");
    }
    let seen = Arc::new(AtomicU64::new(0));
    let probe = Arc::clone(&seen);
    let node = SnubberNode::builder("watch/doctor")
        .policy(
            FaultKind::Panic,
            FaultPolicy::arm(
                FaultAction::Custom(dying_reset),
                LogLevel::Critical,
                "reset",
                "resets_total",
            ),
        )
        .on_fault(move |_, _, _| {
            probe.fetch_add(1, Ordering::SeqCst);
        })
        .build();
    node.report(FaultKind::Panic, 1, 1);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "hook ran despite dead custom"
    );
    // Custom (not TripBreaker) feeds the streak but trips only at 3:
    // two more reports must open the breaker — the dead doctor beheads
    // nothing, the books keep moving.
    node.report(FaultKind::Panic, 2, 2);
    node.report(FaultKind::Panic, 3, 3);
    assert_eq!(node.stats().breaker_state, snubber::BreakerState::HalfOpen);
}

#[test]
fn double_rollback_is_two_calls_zero_harm() {
    static RELEASES: AtomicU64 = AtomicU64::new(0);
    fn release() {
        RELEASES.fetch_add(1, Ordering::SeqCst);
    }
    RELEASES.store(0, Ordering::SeqCst);
    let node = SnubberNode::desktop("watch/double");
    let r = std::panic::catch_unwind(|| {
        let _meta = snubber::SnubberGuard::new(0, 0, release);
        let _app = node.enter(1, 1, release).expect("admit");
        panic!("boom");
    });
    assert!(r.is_err());
    assert_eq!(RELEASES.load(Ordering::SeqCst), 2, "both fired");
    // Hardware free = idempotent release called twice, state still "free".
}
