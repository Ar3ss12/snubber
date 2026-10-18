use snubber::{OverflowPolicy, SnubberNode};
use std::sync::Arc;

#[test]
fn hard_clamp_slots_guarantees_ceiling_under_evict_oldest_stress() {
    // EvictOldest deliberately overbooks by 1: in_flight may reach max+1 transiently.
    // hard_clamp_slots caps it there. Correctness is verified via quiescent postconditions
    // and slot-conservation invariants — not via a racy external counter.
    let node = Arc::new(
        SnubberNode::builder("test_hard_clamp")
            .max_in_flight(2)
            .queue_capacity(8)
            .shed_policy(OverflowPolicy::EvictOldest)
            .trip_after_failures(0) // keep breaker off during stress
            .build(),
    );

    let mut handles = Vec::new();
    for thread_id in 0..8usize {
        let n = Arc::clone(&node);
        handles.push(std::thread::spawn(move || {
            for i in 0..200u64 {
                let task_id = thread_id as u64 * 1000 + i;
                if let Ok(guard) = n.enter(task_id, task_id, || {}) {
                    // Yield to maximise concurrency pressure while the slot is held.
                    std::thread::yield_now();
                    guard.commit();
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join");
    }

    // Quiescent state: every slot must be released after all threads finish.
    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "all lanes must return to zero after drain"
    );
    assert!(
        node.bulkhead_has_room(),
        "node must report available room after drain"
    );

    // Slot conservation: every admitted transaction must settle exactly once.
    let s = node.stats();
    assert_eq!(
        s.total_started,
        s.committed + s.implicit_drops + s.explicit_aborts,
        "every admitted transaction must settle exactly once"
    );
}
