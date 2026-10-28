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
    assert_send_sync::<snubber::GenerationClock>();
    assert_send_sync::<snubber::RestartPolicy>();
    assert_send_sync::<snubber::Backoff>();
    assert_send_sync::<snubber::RestartWindow>();
    assert_send_sync::<snubber::DeathCause>();
    assert_send_sync::<snubber::NoOrphanGuard>();
    assert_send_sync::<snubber::RestartTracker>();
    assert_send_sync::<snubber::PipeDrain>();
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
            let mut accepted = 0u64;
            let mut refused = 0u64;
            for i in 0..ATTEMPTS_PER_THREAD {
                let rb_local = Arc::clone(&rb);
                let guard = match n.probe_erased_auto(move || {
                    rb_local.fetch_add(1, Ordering::SeqCst);
                }) {
                    Ok(g) => {
                        accepted += 1;
                        g
                    }
                    Err(_) => {
                        // Rejection is a valid capacity-shedding state: the
                        // slot was never taken, but the refusal MUST be
                        // counted so the conservation law below can see it.
                        refused += 1;
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
            (accepted, refused)
        }));
    }

    let mut accepted = 0u64;
    let mut refused = 0u64;
    for h in handles {
        let (a, r) = h.join().unwrap();
        accepted += a;
        refused += r;
    }

    // Invariant 1: every attempt is either admitted or shed — no silent
    // third outcome. The node's own books must agree with the harness.
    let attempts = (THREADS * ATTEMPTS_PER_THREAD) as u64;
    assert_eq!(
        accepted + refused,
        attempts,
        "every attempt must be either admitted or counted as shed"
    );
    assert!(
        accepted >= attempts / 10,
        "stress must admit a meaningful volume, not one lucky guard: \
         accepted={accepted} of {attempts}"
    );
    let s = node.stats();
    assert_eq!(
        s.total_started, accepted,
        "node books must match admissions"
    );
    assert_eq!(s.shed_total(), refused, "node books must match shed count");
    // Full seven-term law against the HARNESS count (not against
    // `admission_total()`, which is the sum of the same terms by
    // definition). A refusal hiding in an unbucketed counter breaks this.
    let accounted = s
        .total_started
        .saturating_add(s.bulkhead_rejections)
        .saturating_add(s.stale_epochs_rejected)
        .saturating_add(s.breaker_rejections)
        .saturating_add(s.rate_rejected)
        .saturating_add(s.poisoned_rejected)
        .saturating_add(s.memory_rejections);
    assert_eq!(
        accounted, attempts,
        "every attempt must land in exactly one of the seven admission terms: {s:?}"
    );
    // This node has no rate limit, breaker trip or memory ceiling, so a
    // refusal can only be a bulkhead slot shortage or a stale epoch (auto
    // entries race the shared EpochGate by design and the caller retries).
    // Enumerating the buckets proves no refusal hides in a fifth counter.
    assert_eq!(
        s.bulkhead_rejections
            .saturating_add(s.stale_epochs_rejected),
        refused,
        "refusals must land in the bulkhead or stale-epoch bucket: {s:?}"
    );
    assert_eq!(
        s.rate_rejected
            .saturating_add(s.breaker_rejections)
            .saturating_add(s.memory_rejections)
            .saturating_add(s.poisoned_rejected),
        0,
        "no other shed bucket may fire in this node shape: {s:?}"
    );

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

#[test]
fn static_node_const_capacities_valid() {
    let node = snubber::StaticNode::<4, 16>::build("static_valid");
    assert!(node.bulkhead_has_room());
    // In-flight limit is 4: exactly 4 can be held concurrently with increasing epochs
    let _g1 = node.enter(1, 1, || {}).expect("g1");
    let _g2 = node.enter(2, 2, || {}).expect("g2");
    let _g3 = node.enter(3, 3, || {}).expect("g3");
    let _g4 = node.enter(4, 4, || {}).expect("g4");
    // 5th attempt must be rejected under BulkheadFull
    assert_eq!(
        node.enter(5, 5, || {}).err(),
        Some(snubber::EnterError::BulkheadFull),
        "5th concurrent slot must exceed IN_FLIGHT=4"
    );
}
