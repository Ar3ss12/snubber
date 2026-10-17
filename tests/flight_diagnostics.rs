//! Flight recorder diagnostics: ring buffer, panicking-sink isolation, frame format.
//!
//! Cold paths only (F10): `dump_flight` allocates under a short lock —
//! poll rarely, never per-CPS. Admission (`enter_inner`/`record_locked`)
//! stays ZERO-PANIC: ring slots are `Copy` codes, no `String` inside.

use snubber::{FaultKind, FaultPolicy, FaultRecord, SnubberNode, SnubberStats, TelemetrySink};
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
    assert_eq!(snap.nuclear_breaches(), 2);
    assert_eq!(snap.shed_rate(), 0);
}

#[test]
fn stats_hot_smoke_docs_f10_scope() {
    // 160 CPS stats() loop: passes, documents the F10 cold-path scope
    // (not a perf assert — a smoke that the path stays usable).
    let node = SnubberNode::builder("hot").build();
    for i in 1..=160u64 {
        node.report(FaultKind::StaleEpoch, i, i);
        let _ = node.stats();
    }
    assert_eq!(node.stats().shed_stale(), 160);
}
