//! Regression gate for the `TrackedGuard` settlement leak.
//!
//! `mem::forget(self)` on a settle path suppressed drop glue for the whole
//! guard, so `Arc<Mutex<NodeInner>>` and the captured rollback were never
//! reclaimed (~2.4 KiB per transaction). These tests prove that every
//! settlement path — commit, abort, watchdog timeout, implicit drop and the
//! profiled commit — releases the closure, frees the admission slot and
//! settles exactly once.

use snubber::{SnubberNode, StageKind, TimeoutCheck};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn single_slot(name: &'static str) -> SnubberNode {
    SnubberNode::builder(name).max_in_flight(1).build()
}

fn counter() -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(0))
}

/// A second admission must succeed; it fails if the settled guard leaked
/// its slot or its node reference.
fn assert_slot_reclaimed(node: &SnubberNode, path: &str) {
    let probe = node
        .enter_auto(|| {})
        .unwrap_or_else(|e| panic!("{path}: admission slot was not reclaimed ({e:?})"));
    let _ = probe.commit();
}

#[test]
fn commit_reclaims_closure_and_slot() {
    let node = single_slot("settle/commit");
    let hits = counter();
    let captured = Arc::clone(&hits);
    {
        let guard = node
            .enter_auto(move || {
                captured.fetch_add(1, Ordering::SeqCst);
            })
            .expect("admit");
        assert_eq!(
            Arc::strong_count(&hits),
            2,
            "live guard must own the rollback closure"
        );
        let _ = guard.commit();
    }
    assert_eq!(
        Arc::strong_count(&hits),
        1,
        "commit leaked the rollback closure allocation"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0, "commit must not roll back");
    assert_slot_reclaimed(&node, "commit");
}

#[test]
fn profiled_commit_reclaims_profile_and_slot() {
    let node = single_slot("settle/profiled");
    let hits = counter();
    let captured = Arc::clone(&hits);
    {
        let mut guard = node
            .enter_auto_profiled(move || {
                captured.fetch_add(1, Ordering::SeqCst);
            })
            .expect("admit");
        guard.checkpoint_timed(1, "work", StageKind::Work);
        let (_event, profile) = guard.commit_profiled();
        assert_eq!(
            profile.stage_count, 1,
            "profiled commit must keep the stage"
        );
    }
    assert_eq!(
        Arc::strong_count(&hits),
        1,
        "profiled commit leaked the rollback closure"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_slot_reclaimed(&node, "profiled commit");
}

#[test]
fn abort_reclaims_closure_and_slot() {
    let node = single_slot("settle/abort");
    let hits = counter();
    let captured = Arc::clone(&hits);
    {
        let guard = node
            .enter_auto(move || {
                captured.fetch_add(1, Ordering::SeqCst);
            })
            .expect("admit");
        let _ = guard.abort("planned");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1, "abort must roll back once");
    assert_eq!(
        Arc::strong_count(&hits),
        1,
        "abort leaked the rollback closure allocation"
    );
    assert_slot_reclaimed(&node, "abort");
}

#[test]
fn watchdog_timeout_reclaims_closure_and_slot() {
    let node = SnubberNode::builder("settle/timeout")
        .max_in_flight(1)
        .timeout_ms(1)
        .build();
    let hits = counter();
    let captured = Arc::clone(&hits);
    let guard = node
        .enter_auto(move || {
            captured.fetch_add(1, Ordering::SeqCst);
        })
        .expect("admit");
    std::thread::sleep(Duration::from_millis(5));
    match guard.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {}
        Err(other) => panic!("unexpected watchdog verdict: {other:?}"),
        Ok(_) => panic!("armed 1 ms deadline must fire after 5 ms"),
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "timeout must roll back once"
    );
    assert_eq!(
        Arc::strong_count(&hits),
        1,
        "timeout leaked the rollback closure allocation"
    );
    assert_eq!(node.stats().timeouts_fired, 1);
    assert_slot_reclaimed(&node, "watchdog timeout");
}

#[test]
fn implicit_drop_reclaims_closure_and_slot() {
    let node = single_slot("settle/drop");
    let hits = counter();
    let captured = Arc::clone(&hits);
    {
        let _guard = node
            .enter_auto(move || {
                captured.fetch_add(1, Ordering::SeqCst);
            })
            .expect("admit");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "implicit drop must roll back once"
    );
    assert_eq!(
        Arc::strong_count(&hits),
        1,
        "implicit drop leaked the rollback closure allocation"
    );
    assert_eq!(node.stats().implicit_drops, 1);
    assert_slot_reclaimed(&node, "implicit drop");
}
