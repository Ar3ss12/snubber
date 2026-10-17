//! IRON PROOF: deaf background never forced by Critical.
//!
//! Background holds the only slot and NEVER polls its conduit
//! (no checkpoint — its own enemy, R5). Critical knocks 5x: every
//! knock bounces `BulkheadFull`, exactly ONE `Preempted` is filed,
//! the flag stays up until the deaf worker settles itself, then the
//! retry is admitted. No deadlock, no forced eviction, books balance.

use snubber::{EnterError, Priority, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn critical_never_forces_deaf_background() {
    static ROLLBACKS: AtomicU64 = AtomicU64::new(0);
    static ABORTS: AtomicU64 = AtomicU64::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    ABORTS.store(0, Ordering::SeqCst);

    let node = Arc::new(
        SnubberNode::builder("iron/deaf-bg")
            .max_in_flight(1)
            .queue_limit(64)
            .trip_after_failures(0)
            .build(),
    );

    // Deaf worker: arms Background, sleeps 300 ms WITHOUT ever touching
    // a conduit, then commits. No checkpoint — R5: the node never forces.
    let n_bg = Arc::clone(&node);
    let bg = std::thread::spawn(move || {
        let guard = n_bg
            .enter_with_priority(Priority::Background, 1, 1, || {
                ROLLBACKS.fetch_add(1, Ordering::SeqCst);
            })
            .expect("deaf background arms the slot");
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
    // exactly ONE Preempted filed, flag stays UP (victim is deaf).
    let mut bulk = 0u64;
    for epoch in 2..=6u64 {
        match node.enter_with_priority(Priority::Critical, epoch, epoch, || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        }) {
            Ok(_) => panic!("deaf background must NOT be forced out"),
            Err(EnterError::BulkheadFull) => bulk += 1,
            Err(other) => panic!("unexpected refusal: {other:?}"),
        }
    }
    assert_eq!(bulk, 5, "every knock bounces, none vanish");
    assert!(node.preempt_requested(), "request waits for deaf worker");
    let s = node.stats();
    assert_eq!(s.preempted, 1, "one knock wave = one filed request");
    assert_eq!(s.bulkhead_rejections, 5, "every bounce counted");
    assert_eq!(node.lane_in_flight(), [1, 0, 0], "slot still held");
    assert_eq!(ABORTS.load(Ordering::SeqCst), 0, "nobody aborted deaf");

    // Deaf worker settles ITSELF on its own clock — flag clears, frees.
    bg.join().expect("deaf worker finishes alone");
    assert!(!node.preempt_requested(), "settle of lane 0 clears flag");
    assert_eq!(node.lane_in_flight(), [0, 0, 0]);

    // Retry after the yield: admitted, committed, books balance.
    let g = node
        .enter_with_priority(Priority::Critical, 7, 7, || {
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        })
        .expect("slot freed by the self-settled victim");
    assert_eq!(g.priority(), Priority::Critical);
    g.commit();

    let s = node.stats();
    println!(
        "\n[DEAF]: knocks=5 bulk=5(node={}) preempted={} started={} forced=0 poison={}",
        s.bulkhead_rejections, s.preempted, s.total_started, s.poison_recoveries
    );
    assert_eq!(s.total_started, 2, "bg + critical retry, nothing else");
    assert_eq!(s.bulkhead_rejections, 5);
    assert_eq!(s.preempted, 1, "no duplicate filing while flag was up");
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 0, "clean, no rollback");
    assert_eq!(s.poison_recoveries, 0, "zero-panic holds");
}
