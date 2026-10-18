//! Node tests D: policy alert flag, silent kinds, admission boundary pattern,
//! horizon kind. Public API only.

use snubber::{FaultAction, FaultKind, FaultPolicy, LogLevel, OverflowPolicy, SnubberNode};

#[test]
fn rollback_and_alert_sets_toast_flag() {
    let node = SnubberNode::builder("alerts")
        .policy(
            FaultKind::Timeout,
            FaultPolicy::arm(
                FaultAction::RollbackAndAlert,
                LogLevel::Error,
                "watchdog timeout",
                "watchdog_timeouts_total",
            ),
        )
        .build();
    assert!(!node.take_alert(), "no alert before the fault");
    node.report(FaultKind::Timeout, 9, 9);
    assert!(node.take_alert(), "policy must raise the UI alert");
    assert!(!node.take_alert(), "toast-queue semantics: drains once");
}

#[test]
fn silent_policy_moves_counters_without_noise() {
    let node = SnubberNode::builder("quiet_kind")
        .policy(
            FaultKind::StaleEpoch,
            FaultPolicy::new(FaultAction::DiscardSilently).silent(),
        )
        .build();
    assert_eq!(node.policy_of(FaultKind::StaleEpoch).log_level, None);
    node.report(FaultKind::StaleEpoch, 1, 1);
    assert_eq!(node.reported()[FaultKind::StaleEpoch.index()], 1);
    assert_eq!(node.stats().stale_epochs_rejected, 1);
}

#[test]
fn boundary_example_rejects_poison_before_hardware() {
    // The documented pattern: validate at the admission boundary, name the fault,
    // hardware is never touched.
    fn validate_cps(cps: f64) -> Result<(), &'static str> {
        if !(cps > 0.0 && cps <= 150.0) {
            return Err("cps out of range");
        }
        Ok(())
    }
    let node = SnubberNode::builder("boundary")
        .policy(
            FaultKind::PoisonedPayload,
            FaultPolicy::arm(
                FaultAction::DiscardSilently,
                LogLevel::Error,
                "invalid parameters rejected at admission boundary",
                "poisoned_payloads_total",
            ),
        )
        .build();
    assert!(validate_cps(-5.0).is_err());
    node.report(FaultKind::PoisonedPayload, 1, 1);
    assert_eq!(node.stats().total_started, 0, "no guard armed for poison");
    assert_eq!(node.reported()[FaultKind::PoisonedPayload.index()], 1);
    assert_eq!(
        node.policy_of(FaultKind::PoisonedPayload).metric_tag,
        Some("poisoned_payloads_total")
    );
}

#[test]
fn overflow_policy_surfaces_as_buffer_overflow_kind() {
    // Queue horizon now reports BufferOverflow (the backpressure kind),
    // not a second BulkheadFull — the dashboard tells them apart.
    let node = SnubberNode::builder("horizon")
        .max_in_flight(16)
        .queue_limit(1)
        .overflow(OverflowPolicy::EvictOldest)
        .build();
    let _a = node.enter(1, 1, || {}).expect("first");
    let _b = node.enter(2, 2, || {}).expect("second hits horizon");
    let fault = node.stats().last_fault.expect("horizon fault recorded");
    assert_eq!(fault.kind, FaultKind::BufferOverflow);
}
