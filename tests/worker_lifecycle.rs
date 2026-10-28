//! Worker lifecycle showcase: start → clean stop → immediate restart on a
//! single-flight node. If this is green, snubber never deadlocks threads
//! on restarts: `total_started == committed`, `bulkhead_rejections == 0`.
//! (Unwind accounting: `ErasedGuard` settles its probe clean at admission,
//! so a panicking worker frees its slot with NO phantom `implicit_drop` —
//! the user rollback still runs via `Drop`.)

use snubber::{EnterError, SnubberNode};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// One full worker run: arm at spawn, settle exactly once in the epilogue.
/// Mirrors the Level 2.5 canonical recipe from `docs/USER_GUIDE.md`.
fn run_worker(node: &SnubberNode, clicks: &Arc<AtomicU32>) -> Result<(), EnterError> {
    // `enter_erased_auto` needs `Send + 'static`: clone the Arc in.
    let rescue = Arc::clone(clicks);
    let guard = node.probe_erased_auto(move || {
        // Emergency rollback: would release hardware in a real daemon.
        rescue.fetch_add(1000, Ordering::SeqCst);
    })?;
    // ... the daemon loop would run here ...
    clicks.fetch_add(1, Ordering::SeqCst);
    guard.commit();
    Ok(())
}

#[test]
fn worker_start_stop_restart_never_deadlocks_single_flight() {
    let node = SnubberNode::builder("worker_lifecycle")
        .max_in_flight(1)
        .trip_after_failures(100) // never trip in this test — we count slots, not faults
        .build();
    let clicks = Arc::new(AtomicU32::new(0));

    // Run 1: clean start → clean commit.
    run_worker(&node, &clicks).expect("first run must be admitted");
    // Immediate restart: the committed guard freed the slot, so the second
    // run must be admitted too — no BulkheadFull, no stale slot.
    run_worker(&node, &clicks).expect("immediate restart must be admitted");
    // And a third, for good measure (stop-start-stop shape).
    run_worker(&node, &clicks).expect("third run must be admitted");

    assert_eq!(clicks.load(Ordering::SeqCst), 3, "three clean runs");
    let s = node.stats();
    assert_eq!(s.total_started, 3, "one guard armed per run");
    assert_eq!(s.committed, 3, "every run committed");
    assert_eq!(s.implicit_drops, 0, "no unwind, no abandoned guard");
    assert_eq!(s.bulkhead_rejections, 0, "no slot ever stuck");
}

#[test]
fn worker_panic_rolls_back_and_frees_the_slot() {
    let node = SnubberNode::builder("worker_panic_recovery")
        .max_in_flight(1)
        .trip_after_failures(100)
        .build();
    let rolled = Arc::new(AtomicU32::new(0));

    // A run that dies mid-flight: Drop runs the USER rollback (hardware
    // released). Honest snubber semantics (see `enter_erased` docs): the
    // node books were settled clean by the admission probe, so NO phantom
    // `implicit_drop` is counted — but the SLOT is free for the next run.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let c = Arc::clone(&rolled);
        let _guard = node
            .probe_erased_auto(move || {
                c.fetch_add(1, Ordering::SeqCst);
            })
            .expect("panic-run must be admitted");
        panic!("simulated worker death");
        // NOTE: no commit — Drop fires during unwinding.
    }));
    assert!(r.is_err());
    assert_eq!(rolled.load(Ordering::SeqCst), 1, "rollback ran on unwind");

    // The node must accept the next run: the dead guard's slot is home.
    let clicks = Arc::new(AtomicU32::new(0));
    run_worker(&node, &clicks).expect("run after panic must be admitted");
    assert_eq!(clicks.load(Ordering::SeqCst), 1);
    let s = node.stats();
    assert_eq!(
        s.implicit_drops, 0,
        "erased-guard unwind counts no phantom drop (probe committed clean)"
    );
    assert_eq!(s.bulkhead_rejections, 0, "panic freed its slot");
}

#[test]
fn validate_before_arming_burns_no_slot() {
    // The Layer 2.5 admission boundary rule: a poisoned start reports WITHOUT
    // arming — `total_started` untouched, slot free for the next run.
    let node = SnubberNode::builder("worker_boundary")
        .max_in_flight(1)
        .trip_after_failures(100)
        .build();
    let cps: f64 = -5.0; // poisoned input
    if cps <= 0.0 {
        node.report(snubber::FaultKind::PoisonedPayload, 0, 0);
    } else {
        let _ = run_worker(&node, &Arc::new(AtomicU32::new(0)));
    }
    let s = node.stats();
    assert_eq!(s.total_started, 0, "boundary breach arms no guard");
    assert_eq!(
        s.reported[snubber::FaultKind::PoisonedPayload.index()],
        1,
        "breach reported in the per-kind counter"
    );
    // The slot is still free: a sane run starts normally.
    let clicks = Arc::new(AtomicU32::new(0));
    run_worker(&node, &clicks).expect("sane run after refusal admitted");
    assert_eq!(clicks.load(Ordering::SeqCst), 1);
}
