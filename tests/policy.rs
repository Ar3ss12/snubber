//! Node tests C: Fault Policy Engine — per-kind triad
//! COMMAND + LOG + METRIC. Public API only.

use snubber::{FaultAction, FaultKind, FaultPolicy, LogLevel, SnubberNode};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

#[test]
fn default_table_covers_every_kind() {
    use snubber::DEFAULT_POLICIES;
    assert_eq!(snubber::FaultKind::COUNT, 12);
    assert_eq!(DEFAULT_POLICIES.len(), 12);
    // Spot-check philosophy: lag is Debug, death is Critical.
    assert_eq!(
        DEFAULT_POLICIES[FaultKind::StaleEpoch.index()].log_level,
        Some(LogLevel::Debug)
    );
    assert_eq!(
        DEFAULT_POLICIES[FaultKind::Panic.index()].log_level,
        Some(LogLevel::Critical)
    );
    assert!(matches!(
        DEFAULT_POLICIES[FaultKind::Panic.index()].action,
        FaultAction::TripBreaker
    ));
    // Phase 1 preemption: operational shedding, debug-quiet, own metric column.
    assert_eq!(
        DEFAULT_POLICIES[FaultKind::Preempted.index()].log_level,
        Some(LogLevel::Debug)
    );
    assert_eq!(
        DEFAULT_POLICIES[FaultKind::Preempted.index()].metric_tag,
        Some("preemptions_total")
    );
}

#[test]
fn per_kind_override_replaces_one_cell_only() {
    let node = SnubberNode::builder("matrix")
        .policy(
            FaultKind::StaleEpoch,
            FaultPolicy::arm(
                FaultAction::DiscardSilently,
                LogLevel::Debug,
                "UI lags, packet ignored",
                "stale_packets_dropped",
            ),
        )
        .build();
    let stale = node.policy_of(FaultKind::StaleEpoch);
    assert_eq!(stale.metric_tag, Some("stale_packets_dropped"));
    assert_eq!(stale.log_level, Some(LogLevel::Debug));
    // Neighbour cell untouched.
    let panic = node.policy_of(FaultKind::Panic);
    assert_eq!(panic.log_level, Some(LogLevel::Critical));

    // Closure configurator starts from the table default.
    let node2 = SnubberNode::builder("matrix2")
        .policy_fn(FaultKind::BufferOverflow, |p| {
            p.log(LogLevel::Warn, "buffer full, backpressure applied")
                .metric("buffer_overflow_total")
        })
        .build();
    let buf = node2.policy_of(FaultKind::BufferOverflow);
    assert_eq!(buf.metric_tag, Some("buffer_overflow_total"));
    assert_eq!(buf.log_level, Some(LogLevel::Warn));
}

#[test]
fn custom_command_runs_outside_the_lock() {
    static HEALED: AtomicU32 = AtomicU32::new(0);
    fn heal() {
        HEALED.fetch_add(1, Ordering::SeqCst);
    }
    let seen: Arc<Mutex<Vec<Option<&'static str>>>> = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::clone(&seen);
    let node = SnubberNode::builder("self_heal")
        .policy(
            FaultKind::Panic,
            FaultPolicy::arm(
                FaultAction::Custom(heal),
                LogLevel::Critical,
                "thread crash, driver reset",
                "critical_panics_total",
            ),
        )
        .on_fault(move |_rec, stats, policy| {
            // Hook sees the policy in force + can re-enter stats().
            assert_eq!(stats.reported[FaultKind::Panic.index()], 1);
            probe.lock().unwrap().push(policy.metric_tag);
        })
        .build();

    // Supervisor names the panic explicitly (guard alone can't classify).
    node.report(FaultKind::Panic, 1, 7);
    assert_eq!(HEALED.load(Ordering::SeqCst), 1, "custom cmd must run");
    assert_eq!(node.reported()[FaultKind::Panic.index()], 1);
    let tags = seen.lock().unwrap();
    assert_eq!(tags.as_slice(), [Some("critical_panics_total")]);
}

#[test]
fn trip_breaker_policy_isolates_immediately() {
    let node = SnubberNode::builder("isolate")
        .trip_after_failures(100) // streak would never trip — policy does
        .policy(
            FaultKind::PoisonedPayload,
            FaultPolicy::arm(
                FaultAction::TripBreaker,
                LogLevel::Error,
                "invalid parameters rejected at membrane",
                "poisoned_payloads_total",
            ),
        )
        .build();
    node.report(FaultKind::PoisonedPayload, 1, 1);
    assert_eq!(node.stats().breaker_state, snubber::BreakerState::Open);
    assert_eq!(node.stats().breaker_trips, 1);
}

#[test]
fn fault_action_variant_names() {
    assert_eq!(FaultAction::DiscardSilently.kind_name(), "DiscardSilently");
    assert_eq!(FaultAction::BreakerOpen.kind_name(), "BreakerOpen");
    assert_eq!(
        FaultAction::RollbackAndAlert.kind_name(),
        "RollbackAndAlert"
    );
    assert_eq!(FaultAction::TripBreaker.kind_name(), "TripBreaker");
    assert_eq!(FaultAction::BlockCaller(50).kind_name(), "BlockCaller");
    fn dummy() {}
    assert_eq!(FaultAction::Custom(dummy).kind_name(), "Custom");
}
