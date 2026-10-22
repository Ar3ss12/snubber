//! Phase 2: external memory samples, drain-on-soft, lock-free refusal,
//! v4 snapshot gauges, and the memory/queue anomaly verdicts.

use snubber::{
    classify, find_anomalies_for_snapshot, write_triage, AnomalyKind, AnomalyPolicy, EnterError,
    MemoryVerdict, Refusal, SnapshotV1, SnubberNode, SNAPSHOT_VERSION,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

struct Bomb(Arc<AtomicBool>);
impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn poll_once<F: Future + Unpin>(fut: &mut F) -> Poll<F::Output> {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    Pin::new(fut).poll(&mut cx)
}

#[test]
fn zero_limits_keep_legacy_behaviour() {
    let node = SnubberNode::builder("off").max_in_flight(1).build();
    assert_eq!(node.memory_verdict(), MemoryVerdict::Ok);
    assert_eq!(node.memory_limits_kb(), (0, 0));
    assert_eq!(node.note_memory_kb(999_999), MemoryVerdict::Ok);

    let guard = node.enter(1, 1, || {}).expect("unlimited node admits");
    guard.commit();

    let stats = node.stats();
    assert_eq!(stats.memory_used_kb, 999_999);
    assert_eq!(stats.memory_peak_kb, 999_999);
    assert_eq!(stats.memory_soft_kb, 0);
    assert_eq!(stats.memory_hard_kb, 0);
    assert!(!stats.memory_pressure);
    assert_eq!(stats.memory_rejections, 0);
}

#[test]
fn soft_limit_refuses_admissions_then_recovers() {
    let node = SnubberNode::builder("soft")
        .max_in_flight(4)
        .memory_limits(100, 200)
        .build();
    assert_eq!(node.note_memory_kb(99), MemoryVerdict::Ok);
    let g = node.enter(1, 1, || {}).expect("below soft admits");
    g.commit();

    assert_eq!(node.note_memory_kb(150), MemoryVerdict::Soft);
    assert_eq!(node.memory_verdict(), MemoryVerdict::Soft);

    // Rollback closure is DROPPED, never run, on a pressure refusal.
    let dropped = Arc::new(AtomicBool::new(false));
    let runs = Arc::new(AtomicU64::new(0));
    let bomb = Bomb(Arc::clone(&dropped));
    let runs_ref = Arc::clone(&runs);
    let err = match node.enter(2, 2, move || {
        let _keep = &bomb;
        runs_ref.fetch_add(1, Ordering::SeqCst);
    }) {
        Ok(_) => panic!("soft limit must refuse"),
        Err(e) => e,
    };
    assert_eq!(err, EnterError::MemoryPressure);
    assert_eq!(err.retry_after_ms(), 1000);
    assert_eq!(classify(&err), Refusal::Backoff);
    assert!(dropped.load(Ordering::SeqCst), "closure must be dropped");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "rollback must not run");

    let stats = node.stats();
    assert!(stats.memory_pressure);
    assert_eq!(stats.memory_rejections, 1);
    assert_eq!(stats.memory_used_kb, 150);
    assert_eq!(stats.memory_peak_kb, 150);

    // A later sample below soft clears the flag (cooperative drain).
    assert_eq!(node.note_memory_kb(10), MemoryVerdict::Ok);
    assert!(!node.stats().memory_pressure);
    node.enter(3, 3, || {})
        .expect("admit after recovery")
        .commit();
    assert_eq!(node.stats().memory_rejections, 1);
}

#[test]
fn hard_limit_is_its_own_verdict() {
    let node = SnubberNode::builder("hard")
        .max_in_flight(1)
        .memory_limits(100, 200)
        .build();
    assert_eq!(node.note_memory_kb(250), MemoryVerdict::Hard);
    assert!(node.memory_verdict().is_hard());
    let err = match node.enter(1, 1, || {}) {
        Ok(_) => panic!("hard limit must refuse"),
        Err(e) => e,
    };
    assert_eq!(err, EnterError::MemoryPressure);
    assert!(node.stats().memory_pressure);
}

#[test]
fn async_admission_refuses_before_parking() {
    let node = SnubberNode::builder("async_mem")
        .max_in_flight(1)
        .memory_limits(100, 200)
        .build();
    node.note_memory_kb(120);

    let mut fut = node.enter_async(1, 1, || {});
    assert!(matches!(
        poll_once(&mut fut),
        Poll::Ready(Err(EnterError::MemoryPressure))
    ));
    assert_eq!(node.stats().memory_rejections, 1);
}

#[test]
fn queue_gauges_ride_the_v4_snapshot() {
    let node = SnubberNode::builder("queue")
        .max_in_flight(1)
        .queue_limit(8)
        .memory_limits(100, 200)
        .build();
    let holder = node.enter(1, 1, || {}).expect("fill the only slot");
    let mut parked = node.enter_async(2, 2, || {});
    assert!(poll_once(&mut parked).is_pending());

    node.note_memory_kb(120);
    let snap = node.snapshot_v1();
    assert_eq!(snap.queue_depth, 1);
    assert_eq!(snap.queue_capacity, 8);
    assert_eq!(snap.memory_used_kb, 120);
    assert_eq!(snap.memory_soft_kb, 100);
    assert_eq!(snap.memory_hard_kb, 200);
    assert_eq!(snap.memory_pressure, 1);
    assert_eq!(snap.memory_rejections, 0);

    let bytes = snap.encode();
    assert_eq!(SNAPSHOT_VERSION, 4);
    assert_eq!(&bytes[4..8], &4u32.to_le_bytes());
    assert_eq!(SnapshotV1::decode(&bytes).expect("v4 roundtrip"), snap);

    holder.commit();
}

#[test]
fn memory_and_queue_anomalies_fire_without_min_sample() {
    let snap = SnapshotV1 {
        memory_used_kb: 250,
        memory_peak_kb: 250,
        memory_soft_kb: 100,
        memory_hard_kb: 200,
        memory_pressure: 1,
        queue_depth: 9,
        queue_capacity: 10,
        ..SnapshotV1::default()
    };
    let hits = find_anomalies_for_snapshot("mem", &snap, &AnomalyPolicy::default());

    let growth = hits
        .iter()
        .find(|a| a.kind == AnomalyKind::MemoryGrowth)
        .expect("memory growth verdict");
    assert_eq!(growth.count, 250);
    assert_eq!(growth.pct, 100);
    assert_eq!(AnomalyKind::MemoryGrowth.code(), "memory_growth");

    let queue = hits
        .iter()
        .find(|a| a.kind == AnomalyKind::QueueSaturation)
        .expect("queue saturation verdict");
    assert_eq!(queue.count, 9);
    assert_eq!(queue.pct, 90);
    assert_eq!(AnomalyKind::QueueSaturation.code(), "queue_saturation");

    // A roomy queue and no pressure are silent.
    let calm = SnapshotV1 {
        memory_used_kb: 10,
        memory_soft_kb: 100,
        memory_hard_kb: 200,
        queue_depth: 1,
        queue_capacity: 10,
        ..SnapshotV1::default()
    };
    assert!(find_anomalies_for_snapshot("calm", &calm, &AnomalyPolicy::default()).is_empty());
}

#[test]
fn diff_tracks_memory_window_and_pressure_edge() {
    let before = SnapshotV1 {
        memory_used_kb: 50,
        memory_peak_kb: 60,
        memory_soft_kb: 100,
        memory_hard_kb: 200,
        queue_depth: 0,
        queue_capacity: 8,
        ..SnapshotV1::default()
    };
    let after = SnapshotV1 {
        memory_used_kb: 150,
        memory_peak_kb: 180,
        memory_soft_kb: 100,
        memory_hard_kb: 200,
        memory_rejections: 3,
        memory_pressure: 1,
        queue_depth: 2,
        queue_capacity: 8,
        ..SnapshotV1::default()
    };
    let diff = before.diff(&after);
    assert!(!diff.is_empty());
    assert_eq!(diff.memory_used_kb_from, 50);
    assert_eq!(diff.memory_used_kb_to, 150);
    assert_eq!(diff.memory_peak_kb, 120);
    assert_eq!(diff.memory_rejections, 3);
    assert_eq!(diff.memory_pressure_from, 0);
    assert_eq!(diff.memory_pressure_to, 1);
    assert_eq!(diff.queue_depth_from, 0);
    assert_eq!(diff.queue_depth_to, 2);

    let mut text = String::new();
    diff.write_summary(&mut text).expect("fmt::Write");
    assert!(text.contains("memory 50->150kb peak +120kb reject +3 pressure ok->PRESSURE"));
    assert!(text.contains("queue 0->2"));
}

#[test]
fn triage_card_carries_memory_and_queue_lines() {
    let snap = SnapshotV1 {
        memory_used_kb: 150,
        memory_peak_kb: 160,
        memory_soft_kb: 100,
        memory_hard_kb: 200,
        memory_pressure: 1,
        memory_rejections: 4,
        queue_depth: 8,
        queue_capacity: 8,
        ..SnapshotV1::default()
    };
    let mut text = String::new();
    write_triage(&mut text, "pressured", &snap).expect("fmt::Write");
    assert!(text.contains("verdict=WARN"));
    assert!(text.contains("memory   used=150kb peak=160kb soft=100kb hard=200kb"));
    assert!(text.contains("pressure=PRESSURE rejects=4"));
    assert!(text.contains("queue    depth=8 cap=8 (100%)"));
}
