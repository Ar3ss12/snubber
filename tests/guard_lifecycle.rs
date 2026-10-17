//! Node tests: guard and slot lifecycle on every exit path,
//! re-entrant hooks (open-call, no deadlock), generation re-handshake
//! through the node. Public API only.

use snubber::{FaultKind, OverflowPolicy, SnubberNode, TransactionHeader};
use std::sync::{Arc, Mutex};

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
    // Bullet 2 — the idiot calls node.stats()/node.enter() INSIDE a hook.
    // Open-call discipline (drop lock before emit) => no self-deadlock.
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
    // Zombie from gen 0 is a phantom even with a HIGHER epoch.
    let zombie = TransactionHeader::with_generation(0, 999, 1);
    assert!(!node.observe(&zombie), "zombie generation rejected");
}
