//! Integration tests for `snubber`: real rollback / epoch / unwind behavior.
//!
//! Each test drives the public API only — no access to guard internals.

use snubber::{EpochGate, SnubberEvent, SnubberGuard, TransactionHeader};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

fn counter() -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(0))
}

fn bumped(flag: &Arc<AtomicU32>) -> impl FnOnce() + '_ {
    let flag = Arc::clone(flag);
    move || {
        flag.fetch_add(1, Ordering::SeqCst);
    }
}

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

/// Telemetry payload used by the commit test (Copy: zero-cost by construction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Telemetry {
    cps: u32,
}
