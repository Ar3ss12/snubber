//! Canary differential: naked pass-through leaks, guarded node rescues.
//!
//! Law under test: `SnubberNodeBuilder::passthrough(true)` seals open
//! doors at `build()` — dummy guards never take slots, never touch the
//! breaker or `EpochGate`, never run the rollback; transit counts
//! lock-free in `passthrough_admissions` (own column, never merged).
//! Domain-neutral: abstract atomics only, zero Mouse/GUI words.

use snubber::SnubberNode;
use std::sync::atomic::{AtomicU64, Ordering};

fn real_release(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn naked_run_leaks_and_counts() {
    static HELD: AtomicU64 = AtomicU64::new(0);
    static RELEASED: AtomicU64 = AtomicU64::new(0);
    HELD.store(0, Ordering::SeqCst);
    RELEASED.store(0, Ordering::SeqCst);
    let node = SnubberNode::builder("canary/naked")
        .max_in_flight(4)
        .passthrough(true)
        .build();
    assert!(node.is_passthrough());
    const N: u64 = 50;
    for _ in 0..N {
        HELD.fetch_add(1, Ordering::SeqCst);
        // Naked transit: hold the resource, arm the dummy, then unwind
        // THROUGH the guard scope — Drop runs inside unwinding and must
        // still refuse to roll back.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = node
                .enter_auto(|| real_release(&RELEASED))
                .expect("transit never refuses");
            panic!("canary fault");
        }));
    }
    // Naked: resource stays dirty, rollback never ran, transit counted.
    assert_eq!(HELD.load(Ordering::SeqCst), N, "naked leaks every hold");
    assert_eq!(RELEASED.load(Ordering::SeqCst), 0, "dummy never rolls back");
    let s = node.stats();
    assert_eq!(s.passthrough_admissions, N, "transit column counts");
    assert_eq!(node.passthrough_count(), N);
    // Guarded books untouched: no slots, no drops, breaker shut.
    assert_eq!(s.total_started, 0, "no guarded admission");
    assert_eq!(s.implicit_drops, 0, "no implicit-drop accounting");
    assert_eq!(s.committed, 0);
    assert_eq!(s.bulkhead_rejections, 0);
    assert_eq!(s.stale_epochs_rejected, 0);
    assert!(node.bulkhead_has_room(), "slots never taken");
    println!(
        "\n[CANARY-NAKED]: transit={} held={} released={} started={} drops={}",
        s.passthrough_admissions,
        HELD.load(Ordering::SeqCst),
        RELEASED.load(Ordering::SeqCst),
        s.total_started,
        s.implicit_drops,
    );
}
#[test]
fn guarded_run_rescues() {
    static HELD: AtomicU64 = AtomicU64::new(0);
    static RELEASED: AtomicU64 = AtomicU64::new(0);
    HELD.store(0, Ordering::SeqCst);
    RELEASED.store(0, Ordering::SeqCst);
    let node = SnubberNode::builder("canary/guarded")
        .max_in_flight(4)
        .trip_after_failures(0)
        .build();
    assert!(!node.is_passthrough());
    const N: u64 = 50;
    for _ in 0..N {
        HELD.fetch_add(1, Ordering::SeqCst);
        // Same fault shape as naked: unwind THROUGH the guard scope —
        // guarded Drop MUST run the rollback during unwinding.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = node
                .enter_auto(|| real_release(&RELEASED))
                .expect("slot free");
            panic!("canary fault");
        }));
        HELD.fetch_sub(1, Ordering::SeqCst);
    }
    assert_eq!(
        HELD.load(Ordering::SeqCst),
        0,
        "guarded releases every hold"
    );
    assert_eq!(RELEASED.load(Ordering::SeqCst), N, "rollback ran N times");
    let s = node.stats();
    assert_eq!(s.passthrough_admissions, 0, "guarded never transits");
    assert_eq!(s.total_started, N);
    assert_eq!(s.implicit_drops, N, "drops without commit counted");
    println!(
        "\n[CANARY-GUARDED]: started={} drops={} held={} released={} transit={}",
        s.total_started,
        s.implicit_drops,
        HELD.load(Ordering::SeqCst),
        RELEASED.load(Ordering::SeqCst),
        s.passthrough_admissions,
    );
}

#[test]
fn erased_dummy_never_rolls_back() {
    static HELD: AtomicU64 = AtomicU64::new(0);
    static RELEASED: AtomicU64 = AtomicU64::new(0);
    HELD.store(0, Ordering::SeqCst);
    RELEASED.store(0, Ordering::SeqCst);
    let naked = SnubberNode::builder("canary/erased-naked")
        .passthrough(true)
        .build();
    HELD.fetch_add(1, Ordering::SeqCst);
    let guard = naked
        .enter_erased_auto(|| real_release(&RELEASED))
        .expect("transit never refuses");
    drop(guard);
    assert_eq!(HELD.load(Ordering::SeqCst), 1, "erased dummy leaks");
    assert_eq!(
        RELEASED.load(Ordering::SeqCst),
        0,
        "empty rollback is no-op"
    );
    assert_eq!(naked.stats().passthrough_admissions, 1);
    // Guarded twin cleans the same shape.
    let armed = SnubberNode::builder("canary/erased-armed").build();
    HELD.fetch_add(1, Ordering::SeqCst);
    let guard = armed
        .enter_erased_auto(|| real_release(&RELEASED))
        .expect("slot free");
    drop(guard);
    assert_eq!(
        RELEASED.load(Ordering::SeqCst),
        1,
        "guarded erased rolls back"
    );
}
#[test]
fn transit_touches_no_gate_no_slots() {
    let node = SnubberNode::builder("canary/clean")
        .max_in_flight(2)
        .passthrough(true)
        .build();
    for i in 0..100u64 {
        let guard = node.enter(i, i, || {}).expect("transit never refuses");
        guard.commit();
    }
    let s = node.stats();
    assert_eq!(s.passthrough_admissions, 100);
    assert_eq!(s.total_started, 0);
    assert_eq!(s.stale_epochs_rejected, 0, "gate never observed");
    assert_eq!(s.bulkhead_rejections, 0, "slots never taken");
    assert_eq!(s.committed, 0, "dummy commit writes no books");
    assert!(node.bulkhead_has_room());
}

#[test]
fn wire_stays_512_next_to_transit() {
    let node = SnubberNode::builder("canary/wire")
        .passthrough(true)
        .build();
    for i in 0..10u64 {
        let guard = node.enter(i, i, || {}).expect("transit");
        guard.commit();
    }
    let snap = node.snapshot_v1();
    let bytes = snap.encode();
    assert_eq!(bytes.len(), 512, "wire frame untouched");
    let back = snubber::SnapshotV1::decode(&bytes).expect("CRC ok");
    assert_eq!(back, snap);
    assert_eq!(node.stats().passthrough_admissions, 10);
}
