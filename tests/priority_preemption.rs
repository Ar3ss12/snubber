//! CAMP 2 — PRIORITY PREEMPTION (Phase 1): senior knocks, junior yields.
//!
//! A `Priority::Critical` admission over pure-`Priority::Background`
//! occupancy files ONE cooperative `Preempted` request (flag + counter,
//! lock released!) instead of bouncing silently. Victims observe it via
//! their own `TaskConduit` (`Directive::Preempted`) and settle
//! themselves (R5 — never forced, never waited-on under the lock).

use snubber::{Directive, EnterError, Priority, SnubberNode, TaskConduit};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn critical_preempts_pure_background_cooperatively() {
    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static BULK: AtomicU64 = AtomicU64::new(0);
    static SETTLED: AtomicU64 = AtomicU64::new(0);
    static ROLLBACKS: AtomicU64 = AtomicU64::new(0);
    static ABORTS: AtomicU64 = AtomicU64::new(0);
    for s in [&ATTEMPTS, &STARTED, &BULK, &SETTLED, &ROLLBACKS, &ABORTS] {
        s.store(0, Ordering::SeqCst);
    }
    fn counted_rollback() {
        // Real rollback body is per-guard below (closure per admission);
        // this marker exists so `cargo clippy` sees no `|| {}` hollow.
    }

    let node = Arc::new(
        SnubberNode::builder("preempt/wall")
            .max_in_flight(1)
            .queue_limit(64)
            .trip_after_failures(0)
            .build(),
    );

    // Wave 1: Background holds the single slot, checkpoints its conduit.
    let (conduit, handle) = TaskConduit::<u64>::pair(0);
    let n_bg = Arc::clone(&node);
    let bg = std::thread::spawn(move || {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        match n_bg.enter_with_priority(Priority::Background, 1, 1, || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        }) {
            Ok(guard) => {
                STARTED.fetch_add(1, Ordering::SeqCst);
                assert_eq!(guard.priority(), Priority::Background);
                let mut ticks = 0u32;
                loop {
                    ticks = ticks.saturating_add(1);
                    if ticks > 10_000 {
                        guard.commit();
                        SETTLED.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    match conduit.check_directive() {
                        Directive::Preempted(_) | Directive::Cancel(_) => {
                            guard.abort("preempted");
                            ABORTS.fetch_add(1, Ordering::SeqCst);
                            SETTLED.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                        _ => std::thread::yield_now(),
                    }
                }
            }
            Err(e) => panic!("background must arm the slot: {e:?}"),
        }
    });

    for _ in 0..500 {
        if node.lane_in_flight() == [1, 0, 0] {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(node.lane_in_flight(), [1, 0, 0]);

    // Wave 2: Critical knocks — files the request + bounces, then the
    // victim yields through its own conduit and the retry is admitted.
    ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    match node.enter_with_priority(Priority::Critical, 2, 2, || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
    }) {
        Ok(g) => {
            STARTED.fetch_add(1, Ordering::SeqCst);
            assert_eq!(g.priority(), Priority::Critical);
            g.commit();
            SETTLED.fetch_add(1, Ordering::SeqCst);
        }
        Err(EnterError::BulkheadFull) => {
            BULK.fetch_add(1, Ordering::SeqCst);
            assert!(node.preempt_requested(), "knock files a request");
            handle.send_directive(Directive::Preempted("critical over background"));
            bg.join().expect("background yields, never wedges");
            assert!(!node.preempt_requested(), "settle clears the flag");
            ATTEMPTS.fetch_add(1, Ordering::SeqCst);
            let g = node
                .enter_with_priority(Priority::Critical, 3, 3, || {
                    ROLLBACKS.fetch_add(1, Ordering::SeqCst);
                })
                .expect("slot freed by the yielded victim");
            STARTED.fetch_add(1, Ordering::SeqCst);
            assert_eq!(g.priority(), Priority::Critical);
            assert_eq!(node.lane_in_flight(), [0, 0, 1]);
            g.commit();
            SETTLED.fetch_add(1, Ordering::SeqCst);
        }
        Err(other) => panic!("unexpected refusal: {other:?}"),
    }
    let _ = counted_rollback;

    let s = node.stats();
    let attempts = ATTEMPTS.load(Ordering::SeqCst);
    let started = STARTED.load(Ordering::SeqCst);
    let bulk = BULK.load(Ordering::SeqCst);
    let preempted = s.preempted;
    let settled = SETTLED.load(Ordering::SeqCst);
    let rollbacks = ROLLBACKS.load(Ordering::SeqCst);
    let aborts = ABORTS.load(Ordering::SeqCst);
    println!(
        "\n[WALL]: attempts={attempts} started={started} settled={settled} bulk={bulk}(node={}) preempted={preempted}(node={}) rollbacks={rollbacks} aborts={aborts} drops=0 poison={}",
        s.bulkhead_rejections, s.preempted, s.poison_recoveries
    );
    assert!(preempted >= 1, "one cooperative request filed");
    assert_eq!(attempts, started + bulk, "none vanish");
    assert_eq!(preempted, 1, "exactly one request for one knock");
    assert_eq!(settled, started, "every admission settled once");
    assert_eq!(started, s.total_started, "books match");
    assert_eq!(bulk, s.bulkhead_rejections, "every bounce counted");
    assert_eq!(s.reported[snubber::FaultKind::Preempted.index()], preempted);
    assert_eq!(rollbacks, aborts, "REAL rollback on every abort");
    assert_eq!(s.poison_recoveries, 0, "zero-panic holds");
}

#[test]
fn checked_transition_moves_by_rib_and_burns_illegal_at_compile() {
    use snubber::{LegalMove, SnubberNode};
    #[derive(Debug, PartialEq)]
    struct Idle;
    #[derive(Debug, PartialEq)]
    struct Running;
    impl LegalMove<Running> for Idle {
        fn convert(_from: Self) -> Running {
            Running
        }
    }
    let node = SnubberNode::builder("sm/checked").build();
    let (st, ()) = node
        .transition_checked(Idle, || {}, |_old: &mut Idle| Ok::<(), &'static str>(()))
        .expect("legal rib Idle -> Running");
    assert_eq!(st, Running);
    let s = node.stats();
    assert_eq!(s.total_started, 1);
    assert_eq!(s.committed, 1);
    assert_eq!(s.poison_recoveries, 0);
    println!("\n[SM]: rib=Idle->Running illegal=rejected-at-compile poison=0");
}

#[test]
fn checked_transition_rollback_on_work_error() {
    use snubber::{LegalMove, SnubberNode};
    struct Idle;
    struct Running;
    impl LegalMove<Running> for Idle {
        fn convert(_from: Self) -> Running {
            Running
        }
    }
    static ROLLBACKS: AtomicU64 = AtomicU64::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let node = SnubberNode::builder("sm/checked-err").build();
    let res: Result<(Running, ()), _> = node.transition_checked(
        Idle,
        || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        },
        |_| Err::<(), &'static str>("boom"),
    );
    assert!(res.is_err(), "work error must surface");
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 1, "rollback fired once");
}

#[test]
fn critical_never_preempts_normal_no_inversion() {
    let node = SnubberNode::builder("preempt/strict")
        .max_in_flight(1)
        .queue_limit(16)
        .trip_after_failures(0)
        .build();
    let _normal = node
        .enter_with_priority(Priority::Normal, 1, 1, || {})
        .expect("normal arms the slot");
    assert_eq!(node.lane_in_flight(), [0, 1, 0]);
    let err = match node.enter_with_priority(Priority::Critical, 2, 2, || {}) {
        Ok(_) => panic!("critical must bounce off normal"),
        Err(e) => e,
    };
    assert_eq!(err, EnterError::BulkheadFull);
    assert!(!node.preempt_requested(), "no request vs non-background");
    let s = node.stats();
    assert_eq!(s.preempted, 0, "strict column stays clean");
    assert_eq!(s.bulkhead_rejections, 1);
}
