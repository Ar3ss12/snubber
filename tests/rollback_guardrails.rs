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

use snubber::{EnterError, FaultKind, SnubberError, SnubberNode};
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
            .enter_erased_auto(move || {
                s.store(true, Ordering::SeqCst);
                let nested = rollback_node.enter_erased_auto(|| {});
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
    let stats = node.stats();
    assert_eq!(stats.max_rollback_ms, calm.max_ms, "one number, two views");
    assert_eq!(stats.rollback_stalls, 0, "within budget, never a stall");
}

/// An overrun is a named column, not silence (finding F-07): a body past
/// `rollback_budget_ms` increments `rollback_stalls` exactly once, and the
/// counter measures OVERRUNS — a second, fast body must not add one.
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
    assert_eq!(node.stats().rollback_stalls, 1, "the column is published");

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
    assert_eq!(node.stats().max_rollback_ms, 0);
    assert_eq!(node.stats().rollback_stalls, 0);
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
    if std::env::var_os(CHILD_ENV).is_some() {
        let marker = std::env::var(MARKER_ENV).expect("child gets its marker path");
        let node = SnubberNode::builder("guardrails/wedge/child")
            .max_in_flight(1)
            .build();
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
        return;
    }

    let marker = std::path::PathBuf::from("target")
        .join(format!("rollback_wedge_marker_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let exe = std::env::current_exe().expect("test exe path");
    let mut child = std::process::Command::new(&exe)
        .env(CHILD_ENV, "1")
        .env(MARKER_ENV, &marker)
        .args([
            "--exact",
            "wedged_rollback_wedges_the_node_forever",
            "--ignored",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("wedge child spawns");

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
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "a rollback that re-locks a held mutex must not finish — that wedge is the finding"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&marker);
}
