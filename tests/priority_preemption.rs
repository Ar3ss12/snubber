//! SUITE 02 — PRIORITY PREEMPTION (Phase 1): high-priority preemption and cooperative yielding.
//!
//! A `Priority::Critical` admission over pure-`Priority::Background`
//! occupancy files ONE cooperative `Preempted` request (flag + counter,
//! lock released!) instead of bouncing silently. Victims observe it via
//! their own `TaskConduit` (`Directive::Preempted`) and settle
//! themselves (R5 — never forced, never waited-on under the lock).

use snubber::{Directive, EnterError, Priority, SnubberNode, TaskConduit};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn critical_preempts_pure_background_cooperatively() {
    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static BULK: AtomicU64 = AtomicU64::new(0);
    static SETTLED: AtomicU64 = AtomicU64::new(0);
    static ROLLBACKS: AtomicU64 = AtomicU64::new(0);
    static ABORTS: AtomicU64 = AtomicU64::new(0);
    // Independent resource state: the rollback must actually restore
    // something, not merely bump a counter.
    static BG_RESOURCE_RESTORED: AtomicBool = AtomicBool::new(false);
    for s in [&ATTEMPTS, &STARTED, &BULK, &SETTLED, &ROLLBACKS, &ABORTS] {
        s.store(0, Ordering::SeqCst);
    }
    BG_RESOURCE_RESTORED.store(false, Ordering::SeqCst);

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
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let bg = std::thread::spawn(move || {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        match n_bg.enter_with_priority(Priority::Background, 1, 1, || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
            BG_RESOURCE_RESTORED.store(true, Ordering::SeqCst);
        }) {
            Ok(guard) => {
                STARTED.fetch_add(1, Ordering::SeqCst);
                assert_eq!(guard.priority(), Priority::Background);
                let _ = started_tx.send(());
                loop {
                    match conduit.check_directive() {
                        Directive::Preempted(_) | Directive::Cancel(_) => {
                            guard.abort("preempted");
                            ABORTS.fetch_add(1, Ordering::SeqCst);
                            SETTLED.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                        _ => std::thread::sleep(std::time::Duration::from_millis(1)),
                    }
                }
            }
            Err(e) => panic!("background must arm the slot: {e:?}"),
        }
    });

    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("background started in time");
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
            assert!(
                BG_RESOURCE_RESTORED.load(Ordering::SeqCst),
                "the victim's rollback must restore its resource, not just count"
            );
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
fn checked_transition_moves_by_valid_edge_and_burns_illegal_at_compile() {
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
        .expect("legal edge Idle -> Running");
    assert_eq!(st, Running);
    let s = node.stats();
    assert_eq!(s.total_started, 1);
    assert_eq!(s.committed, 1);
    assert_eq!(s.poison_recoveries, 0);
    println!("\n[SM]: edge=Idle->Running illegal=rejected-at-compile poison=0");
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

#[test]
fn critical_never_forces_uncooperative_worker() {
    static ROLLBACKS: AtomicU64 = AtomicU64::new(0);
    static ABORTS: AtomicU64 = AtomicU64::new(0);
    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    ABORTS.store(0, Ordering::SeqCst);
    ATTEMPTS.store(0, Ordering::SeqCst);

    let node = Arc::new(
        SnubberNode::builder("iron/uncooperative-bg")
            .max_in_flight(1)
            .queue_limit(64)
            .trip_after_failures(0)
            .build(),
    );

    // Uncooperative worker: arms Background, sleeps 300 ms WITHOUT ever touching
    // a conduit, then commits. No checkpoint — R5: the node never forces.
    let n_bg = Arc::clone(&node);
    let bg = std::thread::spawn(move || {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        let guard = n_bg
            .enter_with_priority(Priority::Background, 1, 1, || {
                ROLLBACKS.fetch_add(1, Ordering::SeqCst);
            })
            .expect("uncooperative background arms the slot");
        assert_eq!(guard.priority(), Priority::Background);
        std::thread::sleep(std::time::Duration::from_millis(300));
        guard.commit();
    });

    // Wait until the slot is really held.
    for _ in 0..2000 {
        if node.lane_in_flight() == [1, 0, 0] {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(node.lane_in_flight(), [1, 0, 0]);

    // Five Critical knocks, fresh epochs (monotonic gate): ALL bounce,
    // exactly ONE Preempted filed, flag stays UP (task channel unpolled).
    let mut bulk = 0u64;
    for epoch in 2..=6u64 {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        match node.enter_with_priority(Priority::Critical, epoch, epoch, || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        }) {
            Ok(_) => panic!("uncooperative background must NOT be forced out"),
            Err(EnterError::BulkheadFull) => bulk += 1,
            Err(other) => panic!("unexpected refusal: {other:?}"),
        }
    }
    assert_eq!(bulk, 5, "every knock bounces, none vanish");
    assert!(
        node.preempt_requested(),
        "request waits for uncooperative worker"
    );
    let s = node.stats();
    assert_eq!(s.preempted, 1, "one knock wave = one filed request");
    assert_eq!(s.bulkhead_rejections, 5, "every bounce counted");
    assert_eq!(node.lane_in_flight(), [1, 0, 0], "slot still held");
    assert_eq!(ABORTS.load(Ordering::SeqCst), 0, "nobody aborted worker");

    // Uncooperative worker settles ITSELF on its own clock — flag clears, frees.
    bg.join().expect("uncooperative worker finishes alone");
    assert!(!node.preempt_requested(), "settle of lane 0 clears flag");
    assert_eq!(node.lane_in_flight(), [0, 0, 0]);

    // Retry after the yield: admitted, committed, books balance.
    ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    let g = node
        .enter_with_priority(Priority::Critical, 7, 7, || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        })
        .expect("slot freed by the self-settled victim");
    assert_eq!(g.priority(), Priority::Critical);
    g.commit();

    let s = node.stats();
    assert_eq!(s.total_started, 2, "bg + critical retry, nothing else");
    // Conservation: 1 background + 5 knocks + 1 retry — every attempt
    // lands in exactly one of the two buckets, local and node agree.
    let attempts = ATTEMPTS.load(Ordering::SeqCst);
    assert_eq!(
        attempts,
        s.total_started + s.bulkhead_rejections,
        "no attempt vanished"
    );
    assert_eq!(s.bulkhead_rejections, 5);
    assert_eq!(s.preempted, 1, "no duplicate filing while flag was up");
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 0, "clean, no rollback");
    assert_eq!(s.poison_recoveries, 0, "zero-panic holds");
}
