//! Rollback-phase behavior lock (PLAN_ROLLBACK_GUARDRAILS, Stage 0).
//!
//! Every pre-existing rollback test uses a body that returns immediately
//! (`tests/rollback_panic.rs`, `tests/scoped_transactions.rs`). These tests
//! hold the opposite shape: a body that is *still running* when the rest of
//! the system looks at the node, plus a body that never returns at all.
//!
//! Measured contract this file pins (2026-10-27, HEAD `1513b50`):
//! * a rollback body runs OUTSIDE the node ledger lock (`src/node.rs:5517-5526`
//!   settles only after the body returns), so calling back into the same node
//!   from the rollback is a **refusal**, never a self-deadlock;
//! * the bulkhead slot is held FOR THE WHOLE body, so one body that never
//!   returns turns the node into a wall of `BulkheadFull`;
//! * nothing in the published surface says "a rollback is in progress"
//!   (Stage 3 adds that) — the wedge canary below proves what that costs.
//!
//! A rollback body panics or asserts, the library swallows it under `std`
//! (`src/lib.rs:674-693`). So NO assertion in this file lives inside a
//! rollback body: bodies publish a verdict code into an `AtomicU8`, and the
//! test thread — whose panic is NOT swallowed — reads it.

use snubber::watchdog::poll_rollback;
use snubber::{
    classify, classify_rollback, classify_stall_ceiling, EnterError, FaultAction, FaultKind,
    LogLevel, Refusal, RollbackReentrancy, SnubberError, SnubberNode, StallKind, DEFAULT_POLICIES,
};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// How long a rollback body may take before the test calls it a hang.
const BODY_BUDGET: Duration = Duration::from_millis(1_500);

/// Verdict codes published by a rollback body. `0` means the body never ran,
/// which is itself a failure the test thread detects.
const NOT_RUN: u8 = 0;
const NESTED_ADMITTED: u8 = 1;
const NESTED_REFUSED_BULKHEAD: u8 = 2;
const NESTED_REFUSED_OTHER: u8 = 3;
const NESTED_PARKED: u8 = 4;
const NESTED_REFUSED_REENTRANT: u8 = 5;

/// Run `body` on a throwaway thread and require a verdict within
/// [`BODY_BUDGET`]. A body that never returns fails THIS test instead of
/// hanging the suite: every node here is private to its own test, so the
/// leaked thread cannot poison another case.
fn bounded(label: &'static str, body: impl FnOnce() + Send + 'static) {
    let (tx, rx) = channel::<()>();
    std::thread::spawn(move || {
        body();
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(BODY_BUDGET).is_ok(),
        "{label}: HANG — the scope never returned within 1500 ms"
    );
}

/// Wait until the rollback body actually started, bounded by the same budget.
fn spin_until_started(flag: &AtomicBool) {
    let deadline = std::time::Instant::now() + BODY_BUDGET;
    while !flag.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the rollback body never started"
        );
        std::thread::yield_now();
    }
}

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

/// Does the node still admit and commit ordinary work?
fn node_still_admits(node: &SnubberNode) -> bool {
    node.scoped(|| {}, || Ok::<i32, &'static str>(7)).is_ok()
}

#[test]
fn reentrant_admission_from_sync_rollback_is_admitted_never_hung() {
    // Default capacity: the nested scope is admitted from the rollback body.
    let node = SnubberNode::builder("guardrails/reentry/sync-roomy").build();
    let verdict = Arc::new(AtomicU8::new(NOT_RUN));
    let started = Arc::new(AtomicBool::new(false));
    let (caller, rollback_node) = (node.clone(), node.clone());
    let (v, s) = (Arc::clone(&verdict), Arc::clone(&started));
    bounded("sync re-entry, default capacity", move || {
        let outcome = caller.scoped::<i32, &'static str, _, _>(
            move || {
                s.store(true, Ordering::SeqCst);
                let nested = rollback_node.scoped(|| {}, || Ok::<i32, &'static str>(1));
                v.store(
                    match nested {
                        Ok(_) => NESTED_ADMITTED,
                        Err(SnubberError::Admission(EnterError::BulkheadFull)) => {
                            NESTED_REFUSED_BULKHEAD
                        }
                        Err(_) => NESTED_REFUSED_OTHER,
                    },
                    Ordering::SeqCst,
                );
            },
            || Err::<i32, &'static str>("work failed"),
        );
        assert!(matches!(outcome, Err(SnubberError::Work(_))));
    });
    spin_until_started(&started);
    assert_eq!(
        verdict.load(Ordering::SeqCst),
        NESTED_ADMITTED,
        "measured law: a rollback may re-enter the node — the ledger lock is \
         already released, so the nested admission is granted, not deadlocked"
    );
    assert!(node_still_admits(&node), "the node must keep working");
}

#[test]
fn reentrant_admission_from_sync_rollback_is_refused_when_slot_is_full() {
    // Capacity 1: the rollback still holds the only slot, so the nested
    // admission is REFUSED. That is the honest failure mode — not a deadlock.
    let node = SnubberNode::builder("guardrails/reentry/sync-tight")
        .max_in_flight(1)
        .build();
    let verdict = Arc::new(AtomicU8::new(NOT_RUN));
    let started = Arc::new(AtomicBool::new(false));
    let (caller, rollback_node) = (node.clone(), node.clone());
    let (v, s) = (Arc::clone(&verdict), Arc::clone(&started));
    bounded("sync re-entry, max_in_flight(1)", move || {
        let outcome = caller.scoped::<i32, &'static str, _, _>(
            move || {
                s.store(true, Ordering::SeqCst);
                let nested = rollback_node.scoped(|| {}, || Ok::<i32, &'static str>(1));
                v.store(
                    match nested {
                        Ok(_) => NESTED_ADMITTED,
                        Err(SnubberError::Admission(EnterError::BulkheadFull)) => {
                            NESTED_REFUSED_BULKHEAD
                        }
                        Err(_) => NESTED_REFUSED_OTHER,
                    },
                    Ordering::SeqCst,
                );
            },
            || Err::<i32, &'static str>("work failed"),
        );
        assert!(matches!(outcome, Err(SnubberError::Work(_))));
    });
    spin_until_started(&started);
    assert_eq!(
        verdict.load(Ordering::SeqCst),
        NESTED_REFUSED_BULKHEAD,
        "the rollback still owns the slot, so the nested admission must be a \
         refusal (a retry loop inside the rollback livelocks this thread)"
    );
    assert!(
        node_still_admits(&node),
        "the slot came home after the body"
    );
}

#[test]
fn reentrant_admission_from_erased_rollback_is_granted_by_design() {
    // The erased path is the documented exception: its bulkhead slot is
    // released AT ADMISSION (`settle_admission_only`, src/node.rs:2349 and
    // 5316-5334), and "LIVE erased guards are not bounded by `max_in_flight`"
    // (src/node.rs:2304-2309). So the nested admission is granted even with
    // `max_in_flight(1)` — and a wedged erased rollback wedges only its own
    // thread, not the node's capacity.
    let node = SnubberNode::builder("guardrails/reentry/erased")
        .max_in_flight(1)
        .build();
    let verdict = Arc::new(AtomicU8::new(NOT_RUN));
    let started = Arc::new(AtomicBool::new(false));
    let (caller, rollback_node) = (node.clone(), node.clone());
    let (v, s) = (Arc::clone(&verdict), Arc::clone(&started));
    bounded("erased re-entry", move || {
        let guard = caller
            .probe_erased_auto(move || {
                s.store(true, Ordering::SeqCst);
                let nested = rollback_node.probe_erased_auto(|| {});
                v.store(
                    match nested {
                        Ok(_) => NESTED_ADMITTED,
                        Err(EnterError::BulkheadFull) => NESTED_REFUSED_BULKHEAD,
                        Err(_) => NESTED_REFUSED_OTHER,
                    },
                    Ordering::SeqCst,
                );
            })
            .expect("daemon entry armed the slot");
        // Implicit settle: dropping the armed guard runs the rollback.
        drop(guard);
    });
    spin_until_started(&started);
    assert_eq!(
        verdict.load(Ordering::SeqCst),
        NESTED_ADMITTED,
        "erased admissions release their slot at admission, so the nested \
         call is granted: the ceiling on live erased guards is the caller's"
    );
    assert!(node_still_admits(&node), "the node must keep working");
}

#[test]
fn reentrant_admission_from_async_rollback_parks_instead_of_hanging() {
    // The async path is the third shape: a full bulkhead PARKS the waker in
    // `inner.waiters` instead of refusing (`enter_async_attempt` doc window,
    // src/node.rs:3595-3599). The thread keeps running — but while a rollback
    // body holds the slot and never returns, that waiter is never woken: a
    // wedged rollback converts every later async caller into a permanent
    // `Pending`, until the queue itself answers `QueueFull`.
    let node = SnubberNode::builder("guardrails/reentry/async")
        .max_in_flight(1)
        .build();
    let verdict = Arc::new(AtomicU8::new(NOT_RUN));
    let started = Arc::new(AtomicBool::new(false));
    let (caller, rollback_node) = (node.clone(), node.clone());
    let (v, s) = (Arc::clone(&verdict), Arc::clone(&started));
    bounded("async re-entry", move || {
        let mut future = Box::pin(caller.enter_async_auto(move || {
            s.store(true, Ordering::SeqCst);
            let mut nested = Box::pin(rollback_node.enter_async_auto(|| {}));
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            v.store(
                match nested.as_mut().poll(&mut cx) {
                    Poll::Ready(Err(EnterError::BulkheadFull)) => NESTED_REFUSED_BULKHEAD,
                    Poll::Ready(Ok(_)) => NESTED_ADMITTED,
                    Poll::Ready(Err(_)) => NESTED_REFUSED_OTHER,
                    Poll::Pending => NESTED_PARKED,
                },
                Ordering::SeqCst,
            );
        }));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let guard = match future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(guard)) => guard,
            Poll::Ready(Err(other)) => panic!("async admission rejected: {other:?}"),
            Poll::Pending => panic!("async admission never resolved"),
        };
        // Dropping the armed guard runs the rollback on THIS thread.
        drop(guard);
    });
    spin_until_started(&started);
    assert_eq!(
        verdict.load(Ordering::SeqCst),
        NESTED_PARKED,
        "async re-entry parks a waiter instead of refusing: the thread does \
         not hang, but the parked caller wakes only when the rollback body \
         finishes — measured as `Pending`, never as a deadlock"
    );
    assert!(
        node_still_admits(&node),
        "the slot came home after the body"
    );
}

#[test]
fn slot_is_held_across_the_rollback_body() {
    let node = SnubberNode::builder("guardrails/slot-held")
        .max_in_flight(1)
        .build();
    let entered = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let node = node.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let outcome = node.scoped::<i32, &'static str, _, _>(
                move || {
                    entered.store(true, Ordering::SeqCst);
                    // Park the body until the test says stop. No library
                    // deadline reaches this: `abort` already consumed the
                    // guard, and the slot has not been released yet.
                    let _ = gate_rx.recv();
                },
                || Err::<i32, &'static str>("work failed"),
            );
            assert!(matches!(outcome, Err(SnubberError::Work(_))));
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);

    // While the body is parked, the node refuses everyone else.
    assert!(
        !node.bulkhead_has_room(),
        "the bulkhead slot is held for the whole rollback body"
    );
    let second = node.scoped(|| {}, || Ok::<i32, &'static str>(1));
    assert!(
        matches!(
            second,
            Err(SnubberError::Admission(EnterError::BulkheadFull))
        ),
        "a parked rollback must surface as a refusal, not a hang: {second:?}"
    );
    assert!(
        node.stats().bulkhead_rejections >= 1,
        "the refusal must be counted, not swallowed"
    );
    // MEASURED (2026-10-27): `dump_flight()` is the fault RING
    // (`src/node.rs:1825`, `FlightSlot { kind, epoch, task_id, file, line }`),
    // not a registry of live slots. The refusal does land in it…
    let named = node
        .dump_flight()
        .iter()
        .filter(|slot| matches!(slot.kind, FaultKind::BulkheadFull))
        .count();
    assert!(
        named >= 1,
        "the refusal must be recorded in the fault ring, got {named} BulkheadFull entries"
    );
    // …but NOTHING published says which live slot is currently inside a
    // rollback body (finding F-03, restated after measurement: my probe read
    // the ring as an in-flight registry and mis-called it a lie). The ring
    // stays a fault ring — `rollback_phase()` is the live-slot answer, and
    // it must agree with the bulkhead above.
    let phase = node.rollback_phase();
    assert!(
        phase.active,
        "F-03: a parked rollback must be visible from another thread without \
         the ledger lock — the bulkhead refuses `BulkheadFull` and this view \
         must name why"
    );

    // Let the body finish: the slot comes home and the node works again.
    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");
    assert!(
        node.bulkhead_has_room(),
        "the slot must be released after the rollback body returns"
    );
    assert!(node
        .scoped(|| {}, || Ok::<i32, &'static str>(2))
        .is_ok_and(|v| v == 2));
}

/// The same law on the OTHER settle path: an armed guard dropped without
/// `commit`/`abort` runs the rollback from `Drop`, and only then releases the
/// slot (`src/node.rs:5596-5605`). This is the shape a daemon hits when it
/// forgets to settle — and the shape Stage 0's negative proof flips.
#[test]
fn slot_is_held_across_the_rollback_body_on_the_implicit_drop_path() {
    let node = SnubberNode::builder("guardrails/slot-held-drop")
        .max_in_flight(1)
        .build();
    let entered = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let node = node.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let guard = node
                .enter(1, 1, move || {
                    entered.store(true, Ordering::SeqCst);
                    let _ = gate_rx.recv();
                })
                .expect("the first admission takes the only slot");
            drop(guard); // implicit settle: rollback runs, THEN the slot frees
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);
    assert!(
        !node.bulkhead_has_room(),
        "on the implicit-drop path the slot is still held while the body runs"
    );
    // Coordinates ascend on purpose: `enter` gates stale epochs, and this
    // test mixes manual coordinates with the auto-sequence.
    match node.enter(2, 2, || {}) {
        Err(EnterError::BulkheadFull) => {}
        Err(other) => panic!("a parked rollback on the drop path must refuse: {other:?}"),
        Ok(guard) => {
            drop(guard);
            panic!("the parked rollback still owns the only slot");
        }
    }

    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");
    assert!(
        node.bulkhead_has_room(),
        "the slot must come home after the body returns"
    );
    assert_eq!(node.stats().implicit_drops, 1, "the drop settle was booked");
    node.enter(3, 3, || {})
        .expect("slot released after the rollback body")
        .commit();
}

/// Stage 3: the phase is answerable from ANOTHER thread while the body is
/// still parked — no ledger lock, no allocation, no clock unless a budget
/// is armed. Identity (`epoch`/`task_id`) is what makes the view useful in
/// a post-mortem: it names the transaction the bulkhead is refusing for.
#[test]
fn rollback_phase_is_visible_while_the_body_is_parked() {
    let node = SnubberNode::builder("guardrails/phase/visible")
        .max_in_flight(1)
        .rollback_budget_ms(1_000)
        .build();
    let entered = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let node = node.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let guard = node
                .enter(2, 7, move || {
                    entered.store(true, Ordering::SeqCst);
                    let _ = gate_rx.recv();
                })
                .expect("the first admission takes the only slot");
            drop(guard); // implicit settle runs the parked body
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);
    std::thread::sleep(Duration::from_millis(150));

    let phase = node.rollback_phase();
    assert!(phase.active, "the parked body must be visible: {phase:?}");
    assert!(
        phase.elapsed_ms >= 100,
        "the clock must be running for the OLDEST active body, got {} ms",
        phase.elapsed_ms
    );
    assert_eq!(
        (phase.epoch, phase.task_id),
        (2, 7),
        "the phase must name the transaction holding the slot: {phase:?}"
    );
    assert_eq!(phase.budget_ms, 1_000, "the armed budget is reported");
    assert_eq!(
        phase.stalls, 0,
        "a body inside its budget is not a stall: {phase:?}"
    );

    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");

    let calm = node.rollback_phase();
    assert!(!calm.active, "phase must clear after settle: {calm:?}");
    assert_eq!(calm.elapsed_ms, 0, "no body, no elapsed clock");
    assert!(
        calm.max_ms >= 100,
        "the completed body is billed in max_ms: {calm:?}"
    );
    assert_eq!(calm.stalls, 0, "within budget, never a stall");
    let stats = node.stats();
    assert_eq!(
        (stats.timeouts_fired, stats.deadlock_suspected),
        (0, 0),
        "a rollback body is timed by its own mirror: it must never leak \
         into the guard-owned watchdog columns"
    );
}

/// An overrun is a named column, not silence (finding F-07): a body past
/// `rollback_budget_ms` increments `RollbackPhase::stalls` exactly once, and
/// the counter measures OVERRUNS — a second, fast body must not add one.
#[test]
fn rollback_phase_counts_an_overrun_once() {
    let node = SnubberNode::builder("guardrails/phase/overrun")
        .max_in_flight(1)
        .rollback_budget_ms(20)
        .build();
    let entered = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let node = node.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let guard = node
                .enter(1, 1, move || {
                    entered.store(true, Ordering::SeqCst);
                    let _ = gate_rx.recv();
                })
                .expect("the first admission takes the only slot");
            drop(guard);
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);
    std::thread::sleep(Duration::from_millis(120));
    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");

    let phase = node.rollback_phase();
    assert_eq!(phase.stalls, 1, "one overrun, one stall: {phase:?}");
    assert!(
        phase.max_ms >= 100,
        "the overrun duration is kept, got {} ms",
        phase.max_ms
    );

    // A fast body stays a body: the counter is about overruns, not calls.
    let _ = node.scoped(|| {}, || Err::<i32, &'static str>("work failed"));
    assert_eq!(
        node.rollback_phase().stalls,
        1,
        "a body inside the budget must not inflate the stall column"
    );
}

/// Zero-cost default, measured through behavior: with no budget armed the
/// body is still VISIBLE (`active`), but no clock is stamped — `elapsed_ms`
/// and `max_ms` stay `0` even after a 150 ms body. That is the contract
/// `needs_stamp` keeps for the watchdog, extended to rollbacks.
#[test]
fn phase_reports_activity_without_a_budget_and_stamps_no_clock() {
    let node = SnubberNode::builder("guardrails/phase/no-clock")
        .max_in_flight(1)
        .build();
    let entered = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let node = node.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let guard = node
                .enter(1, 1, move || {
                    entered.store(true, Ordering::SeqCst);
                    let _ = gate_rx.recv();
                })
                .expect("the first admission takes the only slot");
            drop(guard);
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);
    std::thread::sleep(Duration::from_millis(150));
    let phase = node.rollback_phase();
    assert!(phase.active, "activity needs no budget to be visible");
    assert_eq!(phase.budget_ms, 0, "the default budget is disabled");
    assert_eq!(
        phase.elapsed_ms, 0,
        "no budget, no clock stamp — `elapsed_ms` must not invent one"
    );

    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");

    let calm = node.rollback_phase();
    assert!(!calm.active);
    assert_eq!(calm.max_ms, 0, "an unarmed clock cannot bill a duration");
    assert_eq!(calm.stalls, 0);
    assert_eq!(
        (calm.reentries, node.stats().timeouts_fired),
        (0, 0),
        "no budget, no re-entry, and no leak into the guard-owned columns"
    );
}

/// Stage 4: an overrun is not just a counter — it becomes a NAMED fault
/// (`FaultKind::RollbackStall`) carrying the admission coordinates, booked
/// through the same pipeline as every other fault. A body that panics mints
/// no verdict (unwind is not a measurement), and an unarmed budget mints
/// nothing at all.
#[test]
fn budget_exceeded_rolls_back_phase_into_a_named_stall() {
    const BUDGET_MS: u64 = 10;
    let node = SnubberNode::builder("guardrails/budget/named-stall")
        .max_in_flight(1)
        .rollback_budget_ms(BUDGET_MS)
        .build();
    let outcome = node.scoped::<i32, &'static str, _, _>(
        || std::thread::sleep(Duration::from_millis(80)),
        || Err::<i32, &'static str>("work failed"),
    );
    assert!(matches!(outcome, Err(SnubberError::Work(_))));

    let stats = node.stats();
    assert_eq!(
        node.rollback_phase().stalls,
        1,
        "the overrun is counted: {stats:?}"
    );
    assert_eq!(
        stats.reported[FaultKind::RollbackStall.index()],
        1,
        "the overrun must also land in the per-kind column"
    );
    let last = stats.last_fault.as_ref().expect("a named record");
    assert_eq!(last.kind, FaultKind::RollbackStall);
    assert!(
        last.elapsed_ms >= 60,
        "the record carries the measured overrun, got {} ms",
        last.elapsed_ms
    );
    assert_eq!(
        classify_rollback(last.elapsed_ms, BUDGET_MS),
        StallKind::RollbackStall
    );
    assert!(
        node.dump_flight()
            .iter()
            .any(|slot| slot.kind == FaultKind::RollbackStall),
        "the flight ring names the transaction, not just the count"
    );

    // Zero-cost default: the same shape without a budget books no verdict.
    let quiet = SnubberNode::builder("guardrails/budget/quiet").build();
    let _ = quiet.scoped(
        || std::thread::sleep(Duration::from_millis(5)),
        || Err::<i32, &'static str>("work failed"),
    );
    assert_eq!(quiet.rollback_phase().stalls, 0);
    assert!(
        quiet.stats().last_fault.is_none(),
        "no budget, no clock, no verdict"
    );
}

/// Default policy: a body that re-enters its own node is ADMITTED and
/// COUNTED — the 0.6.x behavior, kept byte-for-byte, with a number attached.
#[test]
fn rollback_reentrancy_default_allows_and_counts() {
    let node = SnubberNode::builder("guardrails/reentry/allow-counted")
        .max_in_flight(2)
        .build();
    let verdict = Arc::new(AtomicU8::new(NOT_RUN));
    let (caller, rollback_node) = (node.clone(), node.clone());
    let v = Arc::clone(&verdict);
    bounded("allow policy counts the re-entry", move || {
        let outcome = caller.scoped::<i32, &'static str, _, _>(
            move || {
                let nested = rollback_node.scoped(|| {}, || Ok::<i32, &'static str>(1));
                v.store(
                    match nested {
                        Ok(_) => NESTED_ADMITTED,
                        Err(_) => NESTED_REFUSED_OTHER,
                    },
                    Ordering::SeqCst,
                );
            },
            || Err::<i32, &'static str>("work failed"),
        );
        assert!(matches!(outcome, Err(SnubberError::Work(_))));
    });
    assert_eq!(
        verdict.load(Ordering::SeqCst),
        NESTED_ADMITTED,
        "Allow is the default: the nested admission is granted"
    );
    let stats = node.stats();
    assert_eq!(
        node.rollback_phase().reentries,
        1,
        "the default counts what happened instead of refusing it"
    );
    assert!(
        stats.last_fault.is_none(),
        "an allowed re-entry is not a fault"
    );
}

/// Opt-in `Refuse`: the SAME thread that is inside this node's rollback body
/// gets `EnterError::RollbackReentrant` before any lock — on the sync AND the
/// async path — while other threads keep working.
#[test]
fn rollback_reentrancy_policy_refuse_returns_error() {
    let node = SnubberNode::builder("guardrails/reentry/refuse")
        .max_in_flight(2)
        .rollback_reentrancy(RollbackReentrancy::Refuse)
        .build();
    let sync_verdict = Arc::new(AtomicU8::new(NOT_RUN));
    let async_verdict = Arc::new(AtomicU8::new(NOT_RUN));
    let entered = Arc::new(AtomicBool::new(false));
    let nested_done = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let outer_node = node.clone();
        let body_node = node.clone();
        let entered = Arc::clone(&entered);
        let nested_done = Arc::clone(&nested_done);
        let sv = Arc::clone(&sync_verdict);
        let av = Arc::clone(&async_verdict);
        std::thread::spawn(move || {
            let guard = outer_node
                .enter(1, 1, move || {
                    entered.store(true, Ordering::SeqCst);
                    let nested = body_node.scoped(|| {}, || Ok::<i32, &'static str>(1));
                    sv.store(
                        match nested {
                            Err(SnubberError::Admission(EnterError::RollbackReentrant)) => {
                                NESTED_REFUSED_REENTRANT
                            }
                            Ok(_) => NESTED_ADMITTED,
                            Err(_) => NESTED_REFUSED_OTHER,
                        },
                        Ordering::SeqCst,
                    );
                    let mut fut = Box::pin(body_node.enter_async_auto(|| {}));
                    let waker = noop_waker();
                    let mut cx = Context::from_waker(&waker);
                    av.store(
                        match fut.as_mut().poll(&mut cx) {
                            Poll::Ready(Err(EnterError::RollbackReentrant)) => {
                                NESTED_REFUSED_REENTRANT
                            }
                            Poll::Ready(Ok(_)) => NESTED_ADMITTED,
                            _ => NESTED_REFUSED_OTHER,
                        },
                        Ordering::SeqCst,
                    );
                    nested_done.store(true, Ordering::SeqCst);
                    // Hold the body open so the policy is observed while it runs.
                    let _ = gate_rx.recv();
                })
                .expect("the first admission takes a slot");
            drop(guard);
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);
    let deadline = std::time::Instant::now() + BODY_BUDGET;
    while !nested_done.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the body never finished its nested attempts"
        );
        std::thread::yield_now();
    }

    // Other threads are never affected: the marker is per-thread.
    assert!(
        node.scoped(|| {}, || Ok::<i32, &'static str>(7))
            .is_ok_and(|v| v == 7),
        "Refuse applies to the rollback thread only"
    );
    assert_eq!(
        sync_verdict.load(Ordering::SeqCst),
        NESTED_REFUSED_REENTRANT,
        "the sync path must refuse before touching any lock"
    );
    assert_eq!(
        async_verdict.load(Ordering::SeqCst),
        NESTED_REFUSED_REENTRANT,
        "the async path must resolve the refusal, not park a waiter"
    );
    assert_eq!(
        classify(&EnterError::RollbackReentrant),
        Refusal::BadRequest,
        "a failed-before-lock refusal is the caller's mistake, not backpressure"
    );
    assert!(
        EnterError::RollbackReentrant.backpressure().is_none(),
        "the caller must not retry from inside the same rollback body"
    );
    assert_eq!(
        node.rollback_phase().reentries,
        2,
        "both refused attempts are counted"
    );

    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");
    assert_eq!(
        node.rollback_phase().reentries,
        2,
        "the other thread's admission is not a re-entry"
    );
    assert_eq!(node.rollback_phase().stalls, 0, "a refusal is not a stall");
}

/// Taxonomy lock: `RollbackStall` is the LAST dense fault, Warn-level,
/// non-catastrophic, and only `classify_rollback` can mint it — the watchdog
/// classifier must never report a rollback verdict.
#[test]
fn rollback_stall_kind_is_named_last_and_never_a_deadlock_verdict() {
    assert_eq!(FaultKind::RollbackStall.index() + 1, FaultKind::COUNT);
    assert!(!FaultKind::RollbackStall.is_catastrophic());
    let policy = DEFAULT_POLICIES[FaultKind::RollbackStall.index()];
    assert_eq!(policy.log_level, Some(LogLevel::Warn));
    assert!(matches!(policy.action, FaultAction::DiscardSilently));
    assert_eq!(policy.metric_tag, Some("rollback_stalls_total"));

    assert_eq!(classify_rollback(0, 0), StallKind::WithinBudget);
    assert_eq!(
        classify_rollback(20, 20),
        StallKind::WithinBudget,
        "equal to the budget is inside it"
    );
    assert_eq!(classify_rollback(21, 20), StallKind::RollbackStall);
    assert_eq!(
        classify_rollback(10_000, 0),
        StallKind::WithinBudget,
        "no budget, no verdict"
    );
    assert_ne!(
        classify_stall_ceiling(10_000, 9_000, 1, 1, 10),
        StallKind::RollbackStall,
        "the watchdog classifier never mints the rollback verdict"
    );

    // The 0.6.11 discriminants are caller-visible: any code casting a kind to
    // an integer sees them. `cargo-semver-checks` reports a shift as
    // `enum_no_repr_variant_discriminant_changed` (it caught exactly that when
    // RollbackStall was first inserted between CpuSpin and DeadlockStall);
    // this assertion is the local oracle for the same break.
    assert_eq!(
        [
            StallKind::WithinBudget as u8,
            StallKind::SleepBlocked as u8,
            StallKind::CpuSpin as u8,
            StallKind::DeadlockStall as u8,
            StallKind::RollbackStall as u8,
        ],
        [0, 1, 2, 3, 4],
        "existing StallKind values must never move: declare new kinds LAST"
    );
}

/// Stage 5: the external observer names a body that never returned, and the
/// read books nothing. The node's own `RollbackStall` record is minted only
/// when a body RETURNS, so this verdict is the only one available while it
/// is still running.
#[test]
fn poll_rollback_names_a_body_from_outside_without_touching_the_books() {
    const BUDGET_MS: u64 = 40;
    let node = SnubberNode::builder("guardrails/watchdog/poll")
        .max_in_flight(1)
        .rollback_budget_ms(BUDGET_MS)
        .build();
    let entered = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let node = node.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let guard = node
                .enter(4, 9, move || {
                    entered.store(true, Ordering::SeqCst);
                    let _ = gate_rx.recv();
                })
                .expect("the first admission takes the only slot");
            drop(guard);
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);
    assert_eq!(poll_rollback(&node, 0), None, "no threshold, no verdict");
    assert!(
        poll_rollback(&node, 10_000).is_none(),
        "a body inside the budget is not a stall"
    );

    std::thread::sleep(Duration::from_millis(BUDGET_MS + 120));
    let stall = poll_rollback(&node, BUDGET_MS).expect("the body outlived the budget");
    assert_eq!(
        (stall.epoch, stall.task_id),
        (4, 9),
        "the verdict names the transaction: {stall:?}"
    );
    assert!(
        stall.elapsed_ms > BUDGET_MS,
        "the verdict reports the measured age: {stall:?}"
    );
    assert_eq!(stall.budget_ms, BUDGET_MS);
    assert_eq!(
        node.rollback_phase().stalls,
        0,
        "observing a stall must not book one"
    );
    assert!(
        node.stats().last_fault.is_none(),
        "the reader is not a fault source"
    );

    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");
    assert_eq!(
        node.rollback_phase().stalls,
        1,
        "the returned body books its own overrun exactly once"
    );
}

/// Honest limit: the verdict's clock belongs to the node. Without
/// `rollback_budget_ms(..)` the phase stays `active: true, elapsed_ms: 0`
/// by design, and `poll_rollback` returns `None` instead of inventing age.
#[test]
fn poll_rollback_needs_the_node_clock_armed() {
    let node = SnubberNode::builder("guardrails/watchdog/unarmed")
        .max_in_flight(1)
        .build();
    let entered = Arc::new(AtomicBool::new(false));
    let (gate_tx, gate_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();

    let worker = {
        let node = node.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let guard = node
                .enter(1, 1, move || {
                    entered.store(true, Ordering::SeqCst);
                    let _ = gate_rx.recv();
                })
                .expect("the first admission takes the only slot");
            drop(guard);
            let _ = done_tx.send(());
        })
    };

    spin_until_started(&entered);
    std::thread::sleep(Duration::from_millis(120));
    assert!(node.rollback_phase().active, "the body is visible");
    assert_eq!(node.rollback_phase().elapsed_ms, 0);
    assert_eq!(
        poll_rollback(&node, 1),
        None,
        "an unarmed node clock cannot age a body, so no verdict is minted"
    );

    drop(gate_tx);
    done_rx
        .recv_timeout(Duration::from_millis(3_000))
        .expect("rollback body returned once released");
    worker.join().expect("worker thread finished");
}

/// Kills the wedge child even when an assertion fires first: a failing
/// canary must not leave a deliberately wedged process running.
struct WedgeChild {
    child: std::process::Child,
    scratch: [std::path::PathBuf; 2],
}

impl WedgeChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for WedgeChild {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill_and_wait();
        }
        for path in &self.scratch {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The cost of one wedged rollback, measured out of process.
///
/// The child holds a foreign `Mutex` across the whole scope and re-locks it
/// from the rollback body: a self-deadlock the crate cannot see and cannot
/// cancel (`#![forbid(unsafe_code)]`, and no library has the right to kill a
/// caller thread). The parent proves the body started, then proves the child
/// never leaves it — and kills it. Out of process because the deadlock is
/// real: an in-process version would strand a thread for the whole suite.
#[test]
#[ignore = "wedges a child process on purpose; run via the release ignored lane"]
fn wedged_rollback_wedges_the_node_forever() {
    const CHILD_ENV: &str = "SNUB_ROLLBACK_WEDGE_CHILD";
    const MARKER_ENV: &str = "SNUB_ROLLBACK_WEDGE_MARKER";
    const VERDICT_ENV: &str = "SNUB_ROLLBACK_WEDGE_VERDICT";
    const WEDGE_BUDGET_MS: u64 = 200;
    if std::env::var_os(CHILD_ENV).is_some() {
        let marker = std::env::var(MARKER_ENV).expect("child gets its marker path");
        let verdict = std::env::var(VERDICT_ENV).expect("child gets its verdict path");
        let node = SnubberNode::builder("guardrails/wedge/child")
            .max_in_flight(1)
            .rollback_budget_ms(WEDGE_BUDGET_MS)
            .build();
        // The wedged thread must NOT be the reporter: a second thread of the
        // same child reads the phase atomics and writes the verdict.
        let observer = {
            let node = node.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_millis(5_000);
                while std::time::Instant::now() < deadline {
                    if let Some(stall) = snubber::watchdog::poll_rollback(&node, WEDGE_BUDGET_MS) {
                        let line = format!(
                            "RollbackStall epoch={} task_id={} elapsed_ms={} budget_ms={}\n",
                            stall.epoch, stall.task_id, stall.elapsed_ms, stall.budget_ms
                        );
                        std::fs::write(&verdict, line).expect("child writes the verdict");
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let lock = Arc::new(std::sync::Mutex::new(0u32));
        let held = lock.lock().expect("child takes its own lock");
        let rollback_lock = Arc::clone(&lock);
        let _ = node.scoped::<i32, &'static str, _, _>(
            move || {
                std::fs::write(&marker, b"rollback-body-entered").expect("child writes marker");
                // Same thread, same mutex: this never returns, so the guard
                // under `scoped` never settles and the slot never frees.
                let _wedged = rollback_lock.lock().expect("rollback re-locks");
            },
            || Err::<i32, &'static str>("work failed"),
        );
        drop(held);
        let _ = observer.join();
        return;
    }

    let marker = std::path::PathBuf::from("target")
        .join(format!("rollback_wedge_marker_{}.txt", std::process::id()));
    let verdict = std::path::PathBuf::from("target")
        .join(format!("rollback_wedge_verdict_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&verdict);
    let exe = std::env::current_exe().expect("test exe path");
    let mut child = WedgeChild {
        child: std::process::Command::new(&exe)
            .env(CHILD_ENV, "1")
            .env(MARKER_ENV, &marker)
            .env(VERDICT_ENV, &verdict)
            .args([
                "--exact",
                "wedged_rollback_wedges_the_node_forever",
                "--ignored",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("wedge child spawns"),
        scratch: [marker.clone(), verdict.clone()],
    };

    // The child must reach the rollback body (the scenario is live), then must
    // never leave it (the wedge is real).
    let deadline = std::time::Instant::now() + Duration::from_millis(5_000);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child never entered the rollback body (marker: {})",
            marker.display()
        );
        if let Some(status) = child.try_wait().expect("try_wait") {
            let _ = std::fs::remove_file(&marker);
            panic!("wedge child exited early with {status} — the scenario broke");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Stage 5: hanging is no longer enough — the child must NAME its own
    // wedge from a second thread before the parent kills it.
    let verdict_deadline = std::time::Instant::now() + Duration::from_millis(5_000);
    while !verdict.exists() {
        assert!(
            std::time::Instant::now() < verdict_deadline,
            "the child never named its own wedge (verdict: {})",
            verdict.display()
        );
        if let Some(status) = child.try_wait().expect("try_wait") {
            let _ = std::fs::remove_file(&marker);
            panic!("wedge child exited early with {status} — the observer broke");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let named = std::fs::read_to_string(&verdict).expect("read the child verdict");
    assert!(
        named.starts_with("RollbackStall") && named.contains("budget_ms=200"),
        "the outside verdict must name the stall: {named:?}"
    );
    // Naming the body did not free it: the wedge is still the finding.
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "a rollback that re-locks a held mutex must not finish — that wedge is the finding"
    );
    child.kill_and_wait();
}
