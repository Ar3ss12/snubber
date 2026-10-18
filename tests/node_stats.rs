//! Node tests A: stats, fault coordinates, stale epochs, bulkhead.
//!
//! Public API only. Console reporter stays OFF here (assert via hooks
//! and `stats()` — stderr assertions are brittle and process-global).

use snubber::{EnterError, FaultKind, OverflowPolicy, SnubberNode, TransactionHeader};

fn test_node(name: &'static str) -> SnubberNode {
    SnubberNode::builder(name).build()
}

#[test]
fn commit_path_counts_clean() {
    let node = test_node("commit_clean");
    let g = node.enter(1, 10, || {}).expect("enter");
    assert_eq!(g.header(), TransactionHeader::new(1, 10));
    let done = g.commit();
    assert!(done.is_terminal());
    let s = node.stats();
    assert_eq!(s.total_started, 1);
    assert_eq!(s.committed, 1);
    assert_eq!(s.implicit_drops, 0);
    assert_eq!(s.last_fault, None);
}

#[test]
fn implicit_drop_is_a_bug_report_with_coordinates() {
    use std::sync::{Arc, Mutex};
    let seen: Arc<Mutex<Vec<(FaultKind, String, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::clone(&seen);
    let node = SnubberNode::builder("caller_audit")
        .on_fault(move |rec, _stats, _policy| {
            probe
                .lock()
                .unwrap()
                .push((rec.kind, rec.file.to_string(), rec.line));
        })
        .build();

    {
        let _g = node.enter(1, 99, || {}).expect("enter");
        // no commit -> implicit drop
    }
    let s = node.stats();
    assert_eq!(s.implicit_drops, 1);
    assert_eq!(s.committed, 0);
    let fault = s.last_fault.expect("implicit drop records a fault");
    assert_eq!(fault.kind, FaultKind::ImplicitDrop);
    assert_eq!(fault.node, "caller_audit");
    assert_eq!(fault.header, TransactionHeader::new(1, 99));
    // #[track_caller]: the arming site is THIS file (the `enter` line above).
    assert!(
        fault.file.ends_with("node_stats.rs"),
        "unexpected file: {}",
        fault.file
    );

    let hooks = seen.lock().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].0, FaultKind::ImplicitDrop);
    assert!(hooks[0].1.ends_with("node_stats.rs"));
}

#[test]
fn stale_epoch_never_touches_hardware() {
    let node = test_node("stale_gate");
    let g = node.enter(5, 1, || {}).expect("epoch 5 fresh");
    g.commit();
    match node.enter(5, 1, || {}) {
        Err(EnterError::StaleEpoch) => {}
        _ => panic!("replay must be cut"),
    }
    match node.enter(3, 1, || {}) {
        Err(EnterError::StaleEpoch) => {}
        _ => panic!("reorder must be cut"),
    }
    let s = node.stats();
    assert_eq!(s.total_started, 1, "rejected entries never arm");
    assert_eq!(s.stale_epochs_rejected, 2);
    assert_eq!(s.last_fault.map(|f| f.kind), Some(FaultKind::StaleEpoch));
}

#[test]
fn bulkhead_reject_new_bounds_concurrency() {
    let node = SnubberNode::builder("bulkhead")
        .max_in_flight(1)
        .overflow(OverflowPolicy::RejectNew)
        .build();
    let _held = node.enter(1, 1, || {}).expect("first slot");
    match node.enter(2, 2, || {}) {
        Err(EnterError::BulkheadFull) => {}
        _ => panic!("second must be rejected"),
    }
    let s = node.stats();
    assert_eq!(s.bulkhead_rejections, 1);
    assert_eq!(s.total_started, 1);
}

#[test]
fn bulkhead_evict_oldest_admits_fresh() {
    let node = SnubberNode::builder("evict")
        .max_in_flight(1)
        .overflow(OverflowPolicy::EvictOldest)
        .build();
    let _held = node.enter(1, 1, || {}).expect("first slot");
    let fresh = node.enter(2, 2, || {}).expect("evict admits fresh");
    fresh.commit();
    let s = node.stats();
    assert_eq!(s.evicted, 1);
    assert_eq!(s.total_started, 2);
    assert_eq!(s.committed, 1);
}
