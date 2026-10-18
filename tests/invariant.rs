//! Node tests E: invariant customs gate (`enter_checked`).
//!
//! Poison must be rejected at the admission boundary: no slot, no epoch advance,
//! policy fires with the predicate's static reason. Public API only.

use snubber::{EnterError, FaultAction, FaultKind, FaultPolicy, LogLevel, SnubberNode};

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
    let node = SnubberNode::builder("customs").build();
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
    let node = SnubberNode::builder("customs_zero").build();
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
    use std::sync::atomic::{AtomicU32, Ordering};
    static RESETS: AtomicU32 = AtomicU32::new(0);
    fn reset() {
        RESETS.fetch_add(1, Ordering::SeqCst);
    }
    let node = SnubberNode::builder("customs_cmd")
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
    let node = SnubberNode::builder("customs_site").build();
    let p = ClickParams {
        cps: 50,
        jitter_percent: 150,
    };
    let _ = node.enter_checked(2, 2, &p, physics, || {});
    let fault = node.stats().last_fault.expect("breach recorded");
    assert!(fault.file.ends_with("invariant.rs"), "file: {}", fault.file);
    assert_eq!(
        fault.detail.as_ref().map(|d| d.detail()),
        Some("jitter_percent > 100")
    );
}
