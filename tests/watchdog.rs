//! Fault isolation and panic boundary verification.
//! Tests [`snubber::watchdog::isolate_once`] and fault hook execution contracts.

use snubber::{FaultAction, FaultKind, FaultPolicy, LogLevel, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn isolate_once_catches_panic_and_returns_false() {
    let mut flag = false;
    let ok = snubber::watchdog::isolate_once(|| {
        flag = true;
        panic!("simulated foreign observer failure");
    });

    assert!(!ok, "panic must be intercepted and reported as false");
    assert!(flag, "closure must execute prior to panic");
}

#[test]
fn isolated_chain_continues_execution_after_failure() {
    let mut hook_1_ran = false;
    let mut hook_3_ran = false;

    let chain: [Box<dyn FnOnce()>; 3] = [
        Box::new(|| {
            hook_1_ran = true;
        }),
        Box::new(|| {
            panic!("poisoned observer");
        }),
        Box::new(|| {
            hook_3_ran = true;
        }),
    ];

    let mut success_count = 0;
    for hook in chain {
        if snubber::watchdog::isolate_once(hook) {
            success_count += 1;
        }
    }

    assert_eq!(success_count, 2, "exactly two hooks must succeed");
    assert!(hook_1_ran, "first hook must complete");
    assert!(hook_3_ran, "third hook must execute despite prior panic");
}

#[test]
fn failing_first_hook_does_not_silence_second() {
    let seen = Arc::new(AtomicU64::new(0));
    let probe = Arc::clone(&seen);
    let node = SnubberNode::builder("watch/chain")
        .on_fault(move |_, _, _| {
            probe.fetch_add(1, Ordering::SeqCst);
        })
        .on_fault(|_, _, _| panic!("telemetry plugin died"))
        .build();
    let seen2 = Arc::clone(&seen);
    let node2 = SnubberNode::builder("watch/chain2")
        .on_fault(|_, _, _| panic!("failing first hook"))
        .on_fault(move |_, _, _| {
            seen2.fetch_add(10, Ordering::SeqCst);
        })
        .build();
    let _ = node.enter(1, 1, || {});
    node.report(FaultKind::Panic, 2, 2);
    node2.report(FaultKind::Panic, 3, 3);
    assert_eq!(seen.load(Ordering::SeqCst), 12, "all valid hooks executed");
    assert!(node2.stats().reported[FaultKind::Panic.index()] >= 1);
}

#[test]
fn failing_custom_action_does_not_cancel_diagnosis() {
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
        "hook ran despite failing custom action"
    );
    // Custom (not TripBreaker) feeds the streak but trips only at 3:
    // two more reports must open the breaker.
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
}
