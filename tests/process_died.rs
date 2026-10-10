#[test]
fn process_died_is_own_kind_not_channel() {
    use snubber::{FaultAction, FaultKind, LogLevel, SnubberNode};
    // v0.4 honesty: death ≠ flaky wire. Own index, own policy, own tag.
    assert_eq!(FaultKind::ProcessDied.index(), 10);
    assert!(matches!(
        snubber::DEFAULT_POLICIES[FaultKind::ProcessDied.index()].action,
        FaultAction::RollbackAndAlert
    ));
    assert_eq!(
        snubber::DEFAULT_POLICIES[FaultKind::ProcessDied.index()].log_level,
        Some(LogLevel::Critical)
    );
    assert_eq!(
        snubber::DEFAULT_POLICIES[FaultKind::ProcessDied.index()].metric_tag,
        Some("process_deaths_total")
    );
    // Supervisor reports honestly; alert fires (toast for the operator).
    let node = SnubberNode::builder("death/honest").build();
    node.report(FaultKind::ProcessDied, 1, 1);
    let s = node.stats();
    assert_eq!(s.reported[FaultKind::ProcessDied.index()], 1);
    assert_eq!(s.reported[FaultKind::ChannelDisconnected.index()], 0);
    assert_eq!(s.last_fault.map(|f| f.kind), Some(FaultKind::ProcessDied));
    assert!(node.take_alert(), "death must toast, not whisper");
}
