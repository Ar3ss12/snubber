//! Layer 1: Invariant gates, formal verification harness, and compile-time Send/Sync gates.
//!
//! Merged test suite covering:
//! - Semantic admission boundary gates (`enter_checked`) with pre-admission invariant validation
//! - Mathematical formal invariants under multithreaded contention (zero leaks, single-shot rollback)
//! - Compile-time assertions verifying `Send + Sync` bounds on critical types

use snubber::{
    EnterError, FaultAction, FaultKind, FaultPolicy, LogLevel, Protected, SnapshotV1, SnubberNode,
};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

// ============================================================================
// 1. Compile-Time Send / Sync Assertions
// ============================================================================

const fn assert_send_sync<T: Send + Sync>() {}

const _: () = {
    assert_send_sync::<SnapshotV1>();
};

#[cfg(feature = "std")]
const _: () = {
    assert_send_sync::<SnubberNode>();
    assert_send_sync::<snubber::StateDigest>();
    assert_send_sync::<snubber::ProcessToken>();
    assert_send_sync::<snubber::StaticNode<4, 16>>();
    assert_send_sync::<Protected<snubber::TransactionHeader, fn()>>();
};

// ============================================================================
// 2. Admission Boundary: Invariant Validation (`enter_checked`)
// ============================================================================

struct ClickParams {
    cps: u32,
    jitter_percent: u32,
}

fn physics(p: &ClickParams) -> Result<(), &'static str> {
    if p.cps == 0 || p.cps > 200 {
        return Err("cps == 0 or cps > 200");
    }
    if p.jitter_percent > 100 {
        return Err("jitter_percent > 100");
    }
    Ok(())
}

#[test]
fn valid_params_arm_normally() {
    let node = SnubberNode::builder("invariant_gate").build();
    let p = ClickParams {
        cps: 120,
        jitter_percent: 20,
    };
    node.enter_checked(1, 7, &p, physics, || {})
        .expect("valid")
        .commit();
    let s = node.stats();
    assert_eq!(s.total_started, 1);
    assert_eq!(s.committed, 1);
    assert_eq!(s.poisoned_rejected, 0);
}

#[test]
fn zero_cps_rejected_at_the_admission_boundary() {
    let node = SnubberNode::builder("admission_boundary_zero").build();
    let p = ClickParams {
        cps: 0,
        jitter_percent: 20,
    };
    match node.enter_checked(1, 7, &p, physics, || {}) {
        Err(EnterError::InvariantBreach(reason)) => {
            assert_eq!(reason, "cps == 0 or cps > 200");
        }
        _ => panic!("cps = 0 must breach"),
    }
    let s = node.stats();
    // Hardware untouched: no slot armed, gate not advanced.
    assert_eq!(s.total_started, 0, "poison must not arm a guard");
    assert_eq!(s.poisoned_rejected, 1);
    assert_eq!(s.reported[FaultKind::PoisonedPayload.index()], 1);
    let fault = s.last_fault.expect("breach recorded");
    assert_eq!(fault.kind, FaultKind::PoisonedPayload);
    assert_eq!(
        fault.detail.as_ref().map(|d| d.detail()),
        Some("cps == 0 or cps > 200")
    );
    // Epoch 1 was never admitted — a LATER valid call with epoch 1 works.
    let ok = ClickParams {
        cps: 100,
        jitter_percent: 0,
    };
    node.enter_checked(1, 8, &ok, physics, || {})
        .expect("gate must not have advanced on poison")
        .commit();
    assert_eq!(node.stats().total_started, 1);
}

#[test]
fn breach_fires_poison_policy_with_custom_command() {
    static RESETS: AtomicU32 = AtomicU32::new(0);
    fn reset() {
        RESETS.fetch_add(1, Ordering::SeqCst);
    }
    let node = SnubberNode::builder("admission_boundary_cmd")
        .policy(
            FaultKind::PoisonedPayload,
            FaultPolicy::arm(
                FaultAction::Custom(reset),
                LogLevel::Error,
                "invalid parameters rejected at admission boundary",
                "poisoned_payloads_total",
            ),
        )
        .build();
    let p = ClickParams {
        cps: 999_999,
        jitter_percent: 0,
    };
    let _ = node.enter_checked(4, 1, &p, physics, || {});
    assert_eq!(RESETS.load(Ordering::SeqCst), 1, "policy cmd must run");
    assert!(!node.take_alert(), "DiscardSilent default has no alert");
}

#[test]
fn breach_coordinates_point_at_the_call_site() {
    let node = SnubberNode::builder("admission_boundary_site").build();
    let p = ClickParams {
        cps: 50,
        jitter_percent: 150,
    };
    let _ = node.enter_checked(2, 2, &p, physics, || {});
    let fault = node.stats().last_fault.expect("breach recorded");
    assert!(
        fault.file.ends_with("invariants.rs"),
        "file: {}",
        fault.file
    );
    assert_eq!(
        fault.detail.as_ref().map(|d| d.detail()),
        Some("jitter_percent > 100")
    );
}

// ============================================================================
// 3. Formal Invariant Verification Harness under Stress
// ============================================================================

const THREADS: usize = 4;
const ATTEMPTS_PER_THREAD: usize = 500;

#[test]
fn verify_slot_containment_and_single_shot_rollback() {
    let node = Arc::new(
        SnubberNode::builder("test_invariants")
            .max_in_flight(4)
            .build(),
    );

    let rollback_counter = Arc::new(AtomicU64::new(0));
    let abort_counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    for _ in 0..THREADS {
        let n = Arc::clone(&node);
        let rb = Arc::clone(&rollback_counter);
        let ab = Arc::clone(&abort_counter);

        handles.push(thread::spawn(move || {
            for i in 0..ATTEMPTS_PER_THREAD {
                let rb_local = Arc::clone(&rb);
                let guard = match n.enter_erased_auto(move || {
                    rb_local.fetch_add(1, Ordering::SeqCst);
                }) {
                    Ok(g) => g,
                    Err(_) => {
                        // Rejection is a valid capacity shedding state; no slots leaked
                        continue;
                    }
                };

                // Alternating commit and abort
                if i % 2 == 0 {
                    guard.commit();
                } else {
                    ab.fetch_add(1, Ordering::SeqCst);
                    guard.abort("simulated_failure");
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Invariant 2: Quiescent state must have zero slots in flight
    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "Invariant 2 Broken: In-flight count must strictly return to 0"
    );

    // Invariant 3: Rollbacks must match abort count (no duplicate executions)
    let rollbacks = rollback_counter.load(Ordering::SeqCst);
    let aborts = abort_counter.load(Ordering::SeqCst);
    assert!(
        aborts > 0,
        "Stress test must exercise at least one aborted admission"
    );
    assert_eq!(
        rollbacks, aborts,
        "Invariant 3 Broken: Every aborted admission must trigger its rollback exactly once"
    );
}
