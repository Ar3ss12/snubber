use snubber::{
    find_anomalies_for_snapshot, AnomalyKind, AnomalyPolicy, FaultKind, SnapshotV1, SnubberStats,
};

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
