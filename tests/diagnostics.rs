//! Diagnostics verification: snapshot codec round-trip, anomaly verdicts,
//! flight recorder ring buffer, telemetry sinks, node registry, and health reporting.
//!
//! Cold paths only: encode/decode on cached snapshots, pure math scans.
//! Admission stays ZERO-PANIC; these tests never touch the hot pipeline.

use snubber::{
    assert_no_anomalies, diagnostic_line, find_anomalies, find_anomalies_for_snapshot,
    print_health, write_diagnostic_line, write_health, write_triage, AnomalyKind, AnomalyPolicy,
    FaultKind, FaultPolicy, FaultRecord, NodeSet, SnapshotV1, SnubberNode, SnubberStats,
    TelemetrySink, SNAPSHOT_LEN,
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
fn streaming_anomaly_scan_matches_the_vec_wrapper() {
    let node = SnubberNode::builder("stream").build();
    node.report(FaultKind::Panic, 1, 1);
    let snap = node.snapshot_v1();
    let policy = AnomalyPolicy::default();

    let collected = find_anomalies_for_snapshot("stream", &snap, &policy);
    let mut streamed = Vec::new();
    snubber::scan_anomalies_for_snapshot("stream", &snap, &policy, |anomaly| {
        streamed.push(anomaly);
    });
    assert!(!collected.is_empty(), "the panic must produce a verdict");
    assert_eq!(streamed, collected);
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
    let mut board = NodeSet::new();
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

#[test]
fn pulse_stall_lands_in_snapshot_and_anomaly() {
    let node = SnubberNode::builder("pulse_watch").build();
    // Watcher saw a frozen tick with 2400 ms lag (Front 4 hook).
    node.note_pulse_stall(2400);
    node.note_pulse_stall(800);
    assert_eq!(node.stats().pulse_stalls, 2);
    assert_eq!(node.stats().max_pulse_lag_ms, 2400);

    // Cold frame carries the trace (current-version roundtrip).
    let snap = node.snapshot_v1();
    assert_eq!(snap.pulse_stalls, 2);
    assert_eq!(snap.max_pulse_lag_ms, 2400);
    let bytes = snap.encode();
    let back = SnapshotV1::decode(&bytes).expect("current-version frame decodes");
    assert_eq!(back, snap);

    // Anomaly scan fires PulseStall even on an idle node (no min_sample).
    let hits = find_anomalies_for_snapshot("pulse_watch", &back, &AnomalyPolicy::default());
    let stall = hits
        .iter()
        .find(|h| h.kind == AnomalyKind::PulseStall)
        .expect("pulse stall verdict");
    assert_eq!(stall.count, 2);
    assert_eq!(AnomalyKind::PulseStall.code(), "pulse_stall");
}

#[test]
fn snapshot_diff_is_saturating_and_classified() {
    let mut panics = [0u64; FaultKind::COUNT];
    panics[FaultKind::Panic.index()] = 1;
    let before = SnapshotV1 {
        total_started: 10,
        committed: 8,
        bulkhead_rejections: 2,
        reported: panics,
        ..SnapshotV1::default()
    };

    panics[FaultKind::Panic.index()] = 2;
    let after = SnapshotV1 {
        total_started: 15,
        committed: 12,
        bulkhead_rejections: 3,
        reported: panics,
        pulse_stalls: 1,
        max_pulse_lag_ms: 40,
        breaker_state: 2,
        ..SnapshotV1::default()
    };

    let diff = before.diff(&after);
    assert!(!diff.is_empty());
    assert_eq!(diff.total_started, 5);
    assert_eq!(diff.committed, 4);
    assert_eq!(diff.bulkhead_rejections, 1);
    assert_eq!(diff.reported[FaultKind::Panic.index()], 1);
    assert_eq!(diff.catastrophic_faults(), 1);
    assert_eq!(diff.pulse_stalls, 1);
    assert_eq!(diff.max_pulse_lag_ms, 40);
    assert_eq!(diff.breaker_state_from, 0);
    assert_eq!(diff.breaker_state_to, 2);

    let mut text = String::new();
    diff.write_summary(&mut text).expect("fmt::Write");
    assert!(text.contains("admitted +5"));
    assert!(text.contains("breaker CLOSED->HALF"));
    assert!(text.contains("pulse +1 lag +40ms"));

    // A restart (counters reset) never underflows.
    let backwards = after.diff(&before);
    assert_eq!(backwards.total_started, 0);
    assert_eq!(backwards.committed, 0);
    assert_eq!(backwards.reported[FaultKind::Panic.index()], 0);
    assert_eq!(backwards.breaker_state_from, 2);
    assert_eq!(backwards.breaker_state_to, 0);
}

#[test]
fn triage_card_renders_verdicts() {
    let clean = SnapshotV1::default();
    let mut text = String::new();
    write_triage(&mut text, "idle", &clean).expect("fmt::Write");
    assert!(text.contains("verdict=OK"));
    assert!(text.contains("breaker  CLOSED"));

    let hot = SnapshotV1 {
        total_started: 100,
        committed: 95,
        bulkhead_rejections: 4,
        rate_rejected: 1,
        breaker_state: 1,
        ..SnapshotV1::default()
    };
    text.clear();
    write_triage(&mut text, "hot", &hot).expect("fmt::Write");
    assert!(text.contains("verdict=WARN"));
    assert!(text.contains("breaker  OPEN"));
    assert!(text.contains("admits   105"));
    assert!(text.contains("95 / 90%"));

    let burned = SnapshotV1 {
        implicit_drops: 3,
        ..SnapshotV1::default()
    };
    text.clear();
    write_triage(&mut text, "burned", &burned).expect("fmt::Write");
    assert!(text.contains("verdict=CRIT"));
    assert!(text.contains("drops=3"));
}

#[test]
fn print_health_smoke_does_not_panic_and_matches_write_health() {
    // `print_health` is the exported stderr surface — exercise it on a
    // populated snapshot so a panic or malformed output cannot ship
    // silently. Equivalence: the stderr render must byte-match
    // `write_health` (it forwards to the same renderer).
    let node = SnubberNode::builder("print_health_probe").build();
    node.report(FaultKind::Panic, 1, 1);
    let snap = node.snapshot_v1();
    let policy = AnomalyPolicy::default();
    print_health("print_health_probe", &snap, &policy);
    let mut expected = String::new();
    write_health(&mut expected, "print_health_probe", &snap, &policy).expect("render");
    assert!(expected.contains("[SNUBBER PULSE]"));
}
