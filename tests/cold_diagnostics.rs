//! Phase 3+4: snapshot codec round-trip, anomaly verdicts, registry, macro.
//!
//! Cold paths only: encode/decode on cached snapshots, pure math scans.
//! Admission stays ZERO-PANIC; these tests never touch the hot pipeline.

use snubber::{
    assert_no_anomalies, diagnostic_line, find_anomalies, find_anomalies_for_snapshot,
    write_diagnostic_line, write_health, AnomalyKind, AnomalyPolicy, FaultKind, NodeRegistry,
    SnapshotV1, SnubberNode, SNAPSHOT_LEN,
};

#[test]
fn snapshot_codec_round_trips() {
    let node = SnubberNode::builder("codec").max_in_flight(2).build();
    for i in 1..=10u64 {
        let g = node.enter(i, i, || {}).expect("admit");
        g.commit();
    }
    node.report(FaultKind::StaleEpoch, 100, 100);
    let snap = node.snapshot_v1_in(4);
    assert_eq!(snap.generation, 4);
    let bytes = snap.encode();
    assert_eq!(bytes.len(), SNAPSHOT_LEN);
    assert_eq!(&bytes[0..4], b"SNUB");
    let back = SnapshotV1::decode(&bytes).expect("decode own frame");
    assert_eq!(back, snap);
}

#[test]
fn snapshot_rejects_tampering() {
    let node = SnubberNode::builder("tamper").build();
    let snap = node.snapshot_v1();
    let mut bytes = snap.encode();
    // Flip a payload byte: CRC must fail.
    bytes[42] ^= 0xFF;
    assert!(SnapshotV1::decode(&bytes).is_err());
    // Corrupt magic: hard refusal.
    let mut bad = snap.encode();
    bad[0] = b'X';
    assert!(SnapshotV1::decode(&bad).is_err());
}

#[test]
fn anomalies_pin_each_verdict() {
    // Bottleneck: bulkhead wall at 82%.
    let bulk = SnubberNode::builder("bulk_wall").max_in_flight(1).build();
    let _held = bulk.enter(1, 1, || {}).expect("hold");
    for i in 2..=200u64 {
        let _ = bulk.enter(i, i, || {});
    }
    drop(_held);
    let policy = AnomalyPolicy::default();
    let hits = find_anomalies("bulk_wall", &bulk.stats(), &policy);
    assert!(
        hits.iter().any(|h| h.kind == AnomalyKind::Bottleneck),
        "expected bottleneck, got {hits:?}"
    );

    // Flood: token bucket spam.
    let spam = SnubberNode::builder("spam")
        .rate_per_sec(1)
        .max_in_flight(64)
        .build();
    for i in 1..=200u64 {
        let _ = spam.enter(i, i, || {});
    }
    let hits = find_anomalies("spam", &spam.stats(), &policy);
    assert!(
        hits.iter().any(|h| h.kind == AnomalyKind::Flood),
        "expected flood, got {hits:?}"
    );

    // Desync: stale epoch packet.
    let stale = SnubberNode::builder("stale").build();
    stale.report(FaultKind::StaleEpoch, 1, 1);
    let hits = find_anomalies("stale", &stale.stats(), &policy);
    assert!(hits.iter().any(|h| h.kind == AnomalyKind::Desync));

    // Catastrophic: panic fault.
    let cat_node = SnubberNode::builder("cat_node").build();
    cat_node.report(FaultKind::Panic, 1, 1);
    let hits = find_anomalies("cat_node", &cat_node.stats(), &policy);
    assert!(hits.iter().any(|h| h.kind == AnomalyKind::Catastrophic));

    // Silence: no verdicts on a quiet node.
    let quiet = SnubberNode::builder("quiet").build();
    let g = quiet.enter(1, 1, || {}).expect("admit");
    g.commit();
    assert!(find_anomalies("quiet", &quiet.stats(), &policy).is_empty());
}

#[test]
fn diagnostic_line_and_health_render() {
    let node = SnubberNode::builder("click_worker").build();
    node.report(FaultKind::Panic, 1, 1);
    let snap = node.snapshot_v1();
    let policy = AnomalyPolicy::default();
    let hits = find_anomalies_for_snapshot("click_worker", &snap, &policy);
    let cat = hits
        .iter()
        .find(|h| h.kind == AnomalyKind::Catastrophic)
        .expect("catastrophic");
    let line = diagnostic_line("click_worker", cat, &snap);
    assert!(line.starts_with("[DIAG 0x"), "got {line}");
    assert!(line.contains("catastrophic_fault"));

    let mut buf = String::new();
    write_health(&mut buf, "click_worker", &snap, &policy).expect("render");
    assert!(buf.contains("[SNUBBER PULSE]"));
    assert!(buf.contains("CRITICAL: 1 breaches"));

    let mut diag = String::new();
    write_diagnostic_line(&mut diag, "click_worker", cat, &snap).expect("diag");
    assert_eq!(diag, line);
}

#[test]
fn registry_boards_explicit_handles() {
    let click = SnubberNode::builder("click").build();
    let hotkey = SnubberNode::builder("hotkey").build();
    hotkey.report(FaultKind::Panic, 1, 1);
    let mut board = NodeRegistry::new();
    assert!(board.is_empty());
    board.register("click", &click);
    board.register("hotkey", &hotkey);
    assert_eq!(board.len(), 2);
    let snaps = board.snapshots();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].0, "click");
    let policy = AnomalyPolicy::default();
    let hits = board.find_all_anomalies(&policy);
    assert!(hits.iter().any(|h| h.node == "hotkey"));
    board.clear();
    assert!(board.is_empty());
}

#[test]
fn assert_macro_is_green_on_quiet_red_on_sick() {
    let quiet = SnubberNode::builder("macro_quiet").build();
    let g = quiet.enter(1, 1, || {}).expect("admit");
    g.commit();
    assert_no_anomalies!(&quiet);

    let sick = SnubberNode::builder("macro_sick").build();
    sick.report(FaultKind::Panic, 1, 1);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_anomalies!(&sick);
    }));
    assert!(caught.is_err(), "macro must fail red on catastrophic fault");
}
