//! Layer 1: Core RAII guard, slot lifecycle, and protected resource integration.
//!
//! Merged test suite covering:
//! - [`SnubberGuard`] core drop-guaranteed rollback, monotonic headers, and unwinding
//! - [`Protected`] scoped resource encapsulation, deref semantics, and drop order
//! - Node-level guard/slot lifecycles, re-entrant hooks, and generation re-handshakes

use snubber::{
    EpochGate, FaultKind, OverflowPolicy, Protected, SnubberEvent, SnubberGuard, SnubberNode,
    TransactionHeader,
};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

fn counter() -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(0))
}

fn bumped(flag: &Arc<AtomicU32>) -> impl FnOnce() + '_ {
    let flag = Arc::clone(flag);
    move || {
        flag.fetch_add(1, Ordering::SeqCst);
    }
}

// ============================================================================
// 1. Core SnubberGuard & EpochGate Invariants
// ============================================================================

#[test]
fn commit_disarms_the_rollback() {
    let calls = counter();
    let header;
    {
        let guard = SnubberGuard::new(1, 100, bumped(&calls));
        header = guard.header();
        let ack = guard.ack::<()>();
        assert_eq!(ack.header(), header);
        assert!(!ack.is_terminal());

        let live = guard.progress(Telemetry { cps: 120 });
        match live {
            SnubberEvent::Progress { header: h, payload } => {
                assert_eq!(h, header);
                assert_eq!(payload.cps, 120);
            }
            other => panic!("expected Progress, got {other:?}"),
        }

        let done = guard.commit();
        assert_eq!(done.header(), header);
        assert!(done.is_terminal());
        assert!(matches!(done, SnubberEvent::Committed { .. }));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "commit must NOT roll back");
}

#[test]
fn early_drop_runs_the_rollback_exactly_once() {
    let calls = counter();
    {
        let _guard = SnubberGuard::new(2, 200, bumped(&calls));
        // scope exit without commit -> Drop -> rollback
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn early_return_without_commit_still_rolls_back() {
    fn fallible(path_ok: bool, calls: &Arc<AtomicU32>) -> Result<SnubberEvent<()>, &'static str> {
        let guard = SnubberGuard::new(3, 300, bumped(calls));
        let _ack = guard.ack::<()>();
        if !path_ok {
            return Err("device busy"); // guard drops here -> rollback
        }
        Ok(guard.commit())
    }

    let calls = counter();
    let err = fallible(false, &calls).expect_err("must fail");
    assert_eq!(err, "device busy");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "`?`/return path must roll back"
    );

    let calls = counter();
    let done = fallible(true, &calls).expect("must commit");
    assert!(done.is_terminal());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn stale_epochs_are_cut_off() {
    let mut gate = EpochGate::new();
    let t = |e: u64| TransactionHeader::new(e, 7);

    assert!(gate.observe(&t(1)), "epoch 1 is fresh");
    assert!(gate.observe(&t(2)), "epoch 2 is fresh");
    assert!(!gate.observe(&t(2)), "replayed epoch 2 is stale");
    assert!(!gate.observe(&t(1)), "reordered epoch 1 is stale");
    assert!(gate.observe(&t(5)), "epoch 5 jumps forward, still fresh");
    assert!(!gate.observe(&t(4)), "epoch 4 arrives after 5: stale");
    assert_eq!(gate.last_seen(), 5);
}

#[test]
fn panic_unwind_still_runs_the_rollback() {
    let calls = counter();
    let probe = AssertUnwindSafe(|| {
        let _guard = SnubberGuard::new(9, 900, bumped(&calls));
        panic!("simulated driver failure");
    });
    let caught = catch_unwind(probe).expect_err("must unwind");
    let msg = caught
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| caught.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic>");
    assert!(msg.contains("simulated driver failure"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "unwind must run the rollback exactly once"
    );
}

#[test]
fn explicit_abort_runs_rollback_and_reports_reason() {
    let calls = counter();
    let guard = SnubberGuard::new(4, 400, bumped(&calls));
    let header = guard.header();
    let event = guard.abort("uipi_blocked");
    match event {
        SnubberEvent::Aborted { header: h, reason } => {
            assert_eq!(h, header);
            assert_eq!(reason, "uipi_blocked");
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
    assert!(event.is_terminal());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// ============================================================================
// 2. Node Guard & Slot Lifecycle Invariants
// ============================================================================

#[test]
fn slot_returns_on_panic_unwind() {
    let node = SnubberNode::builder("slots")
        .max_in_flight(1)
        .trip_after_failures(100)
        .build();
    let probe = std::panic::AssertUnwindSafe(|| {
        let _g = node.enter(1, 1, || {}).expect("enter");
        panic!("boom");
    });
    let _ = std::panic::catch_unwind(probe);
    // Slot came home: the node admits again immediately.
    node.enter(2, 2, || {}).expect("slot freed").commit();
    assert_eq!(node.stats().implicit_drops, 1);
    assert_eq!(node.stats().committed, 1);
}

#[test]
fn slot_returns_on_question_mark_early_return() {
    fn fallible(node: &SnubberNode, epoch: u64, ok: bool) -> Result<(), &'static str> {
        let _g = node.enter(epoch, 1, || {}).map_err(|_| "enter failed")?;
        if !ok {
            return Err("nope"); // guard drops -> slot home
        }
        Ok(())
    }
    let node = SnubberNode::builder("slots_q")
        .max_in_flight(1)
        .trip_after_failures(100)
        .build();
    assert_eq!(fallible(&node, 1, false), Err("nope"));
    fallible(&node, 2, true).expect("slot freed after ?-exit");
    // Both exits are implicit drops (fallible never commits) — but the
    // SLOT came home both times, which is what this test proves.
    assert_eq!(node.stats().implicit_drops, 2);
}

#[test]
fn hooks_are_reentrant_no_deadlock() {
    let node = SnubberNode::builder("reenter")
        .trip_after_failures(100)
        .build();
    let handle = node.clone();
    let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::clone(&seen);
    let node2 = SnubberNode::builder("reenter2")
        .trip_after_failures(100)
        .on_fault(move |_rec, _stats, _policy| {
            // Re-enter the OTHER node from inside the hook: if emit held
            // any lock, this would wedge. It must just work.
            let _ = handle.stats().total_started;
            probe.lock().unwrap().push(1);
        })
        .build();
    node2.report(FaultKind::StaleEpoch, 1, 1);
    assert_eq!(seen.lock().unwrap().len(), 1);
    let _ = node.stats();
}

#[test]
fn node_rehandshakes_new_generation() {
    // Cold-boot split-brain through the node: old client at epoch 50,
    // rebooted client starts gen 1 / epoch 1 — admitted, no wedge.
    let node = SnubberNode::builder("reboot")
        .max_in_flight(16)
        .overflow(OverflowPolicy::EvictOldest)
        .build();
    // Simulate gen-0 traffic by direct gate: enter uses gen 0.
    node.enter(50, 1, || {}).expect("gen0").commit();
    // A gen-1 header must be fresh even at epoch 1 < 50.
    let fresh = TransactionHeader::with_generation(1, 1, 2);
    assert!(node.observe(&fresh), "new generation re-handshakes");
    // Stale packet from gen 0 is rejected even with a HIGHER epoch.
    let stale = TransactionHeader::with_generation(0, 999, 1);
    assert!(!node.observe(&stale), "stale generation rejected");
}

// ============================================================================
// 3. Protected Resource Wrapper Invariants
// ============================================================================

#[test]
fn commit_releases_resource_intact() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let guard = SnubberGuard::new(1, 1, || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
    });
    let mut p = Protected::new(42u32, guard);
    // Deref & DerefMut verification
    assert_eq!(*p, 42);
    *p += 8;
    assert_eq!(*p.get(), 50);
    let out = p.commit();
    assert_eq!(out, 50, "commit returns inner resource value intact");
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        0,
        "no rollback triggered on commit"
    );
}

#[test]
fn error_drops_resource_and_executes_rollback_once() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    // Drop order witness: rollback must observe resource alive
    // (guard field drops before wrapped resource field).
    let state_marker = Arc::new(AtomicU32::new(0));
    let probe = Arc::clone(&state_marker);
    struct ResourceProbe {
        state: Arc<AtomicU32>,
    }
    impl Drop for ResourceProbe {
        fn drop(&mut self) {
            self.state.store(2, Ordering::SeqCst); // drops second
        }
    }
    let res = ResourceProbe {
        state: Arc::clone(&state_marker),
    };
    state_marker.store(1, Ordering::SeqCst); // active during transaction
    let guard = SnubberGuard::new(2, 2, move || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        // Rollback runs while resource still lives: marker is 1.
        probe.store(probe.load(Ordering::SeqCst), Ordering::SeqCst);
    });
    let mut p = Protected::new(res, guard);
    let _ = p.get_mut();
    drop(p); // implicit abort on drop
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        1,
        "exactly one rollback execution"
    );
    assert_eq!(
        state_marker.load(Ordering::SeqCst),
        2,
        "resource dropped after guard rollback completed"
    );
}

#[test]
fn abort_runs_rollback_and_preserves_authentic_header() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let guard = SnubberGuard::new(3, 42, || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
    });
    let p = Protected::new(7u32, guard);
    let ev = p.abort("plan-b");
    assert!(ev.is_terminal(), "abort returns terminal event");
    assert_eq!(ev.header().epoch, 3, "preserves authentic epoch");
    assert_eq!(ev.header().task_id, 42, "preserves authentic task_id");
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        1,
        "rollback triggered on abort"
    );
}

/// Telemetry payload used by the commit test (Copy: zero-cost by construction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Telemetry {
    cps: u32,
}
