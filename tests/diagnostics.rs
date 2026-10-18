//! Diagnostics verification: snapshot codec round-trip, anomaly verdicts,
//! flight recorder ring buffer, telemetry sinks, node registry, and health reporting.
//!
//! Cold paths only: encode/decode on cached snapshots, pure math scans.
//! Admission stays ZERO-PANIC; these tests never touch the hot pipeline.

use snubber::{
    assert_no_anomalies, diagnostic_line, find_anomalies, find_anomalies_for_snapshot,
    write_diagnostic_line, write_health, AnomalyKind, AnomalyPolicy, FaultKind, FaultPolicy,
    FaultRecord, NodeRegistry, SnapshotV1, SnubberNode, SnubberStats, TelemetrySink, SNAPSHOT_LEN,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

struct Counter(Arc<AtomicU64>);
impl TelemetrySink for Counter {
    fn on_fault(&self, _r: &FaultRecord, _s: &SnubberStats, _p: &FaultPolicy) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct PanickingSink;
impl TelemetrySink for PanickingSink {
    fn on_fault(&self, _r: &FaultRecord, _s: &SnubberStats, _p: &FaultPolicy) {
        panic!("telemetry plugin simulated failure");
    }
}

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
fn snapshot_v1_codec_roundtrip_with_catastrophic_anomalies() {
    let mut stats = SnubberStats::default();
    stats.reported[FaultKind::Panic.index()] = 3;
    stats.stale_epochs_rejected = 1;

    let snap = SnapshotV1::from_stats(&stats, 42);
    assert_eq!(snap.catastrophic_faults(), 3);

    // Verify binary roundtrip:
    let bytes = snap.encode();
    let decoded = SnapshotV1::decode(&bytes).expect("CRC and version must hold");
    assert_eq!(decoded.catastrophic_faults(), 3);
    assert_eq!(decoded.generation, 42);

    // Verify diagnostic report generation with standardized terminology:
    let anomalies = find_anomalies_for_snapshot("worker", &decoded, &AnomalyPolicy::default());
    assert!(anomalies
        .iter()
        .any(|a| a.kind == AnomalyKind::Catastrophic));
    assert_eq!(AnomalyKind::Catastrophic.code(), "catastrophic_fault");
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
fn assert_macro_is_green_on_quiet_red_on_faulty() {
    let quiet = SnubberNode::builder("macro_quiet").build();
    let g = quiet.enter(1, 1, || {}).expect("admit");
    g.commit();
    assert_no_anomalies!(&quiet);

    let faulty = SnubberNode::builder("macro_faulty").build();
    faulty.report(FaultKind::Panic, 1, 1);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_anomalies!(&faulty);
    }));
    assert!(caught.is_err(), "macro must fail red on catastrophic fault");
}

#[test]
fn ring_holds_last_n_codes() {
    let node = SnubberNode::builder("flight").flight_depth(16).build();
    assert_eq!(node.flight_depth(), 16);
    for i in 1..=20u64 {
        node.report(FaultKind::StaleEpoch, i, i);
    }
    let dump = node.dump_flight();
    assert_eq!(dump.len(), 16, "cap 16 evicts oldest 4");
    assert_eq!(dump[0].epoch, 5, "oldest-first after eviction");
    assert_eq!(dump[15].epoch, 20);
    assert!(dump.iter().all(|s| s.kind == FaultKind::StaleEpoch));
    // Implicit drops land in the ring too (guard path).
    let g = node.enter(100, 100, || {}).expect("admit");
    drop(g);
    let dump2 = node.dump_flight();
    assert_eq!(dump2.len(), 16);
    assert_eq!(dump2[15].kind, FaultKind::ImplicitDrop);
    assert_eq!(node.stats().poison_recoveries, 0);
}

#[test]
fn flight_depth_clamps() {
    let tiny = SnubberNode::builder("tiny").flight_depth(0).build();
    assert_eq!(tiny.flight_depth(), 1);
    let huge = SnubberNode::builder("huge").flight_depth(9999).build();
    assert_eq!(huge.flight_depth(), 64);
}

#[test]
fn panicking_sink_does_not_break_chain() {
    let hits = Arc::new(AtomicU64::new(0));
    let node = SnubberNode::builder("sinks")
        .telemetry_sink(PanickingSink)
        .telemetry_sink(Counter(Arc::clone(&hits)))
        .build();
    node.report(FaultKind::Panic, 1, 1);
    node.report(FaultKind::Timeout, 2, 2);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "good sink still fires");
    assert_eq!(node.stats().poison_recoveries, 0);
    // inspect() is the stats() alias.
    let snap = node.inspect();
    assert_eq!(snap.catastrophic_faults(), 2);
    assert_eq!(snap.shed_rate(), 0);
}

#[test]
fn stats_hot_smoke_docs_f10_scope() {
    let node = SnubberNode::builder("hot").build();
    for i in 1..=160u64 {
        node.report(FaultKind::StaleEpoch, i, i);
        let _ = node.stats();
    }
    assert_eq!(node.stats().shed_stale(), 160);
}
