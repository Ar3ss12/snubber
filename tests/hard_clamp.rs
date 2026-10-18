use snubber::{OverflowPolicy, SnubberNode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn hard_clamp_slots_guarantees_ceiling_under_evict_oldest_stress() {
    // max_in_flight = 2. EvictOldest allows intentional overbooking by +1,
    // so legal ceiling is max + 1 = 3.
    let max = 2;
    let node = Arc::new(
        SnubberNode::builder("test_hard_clamp")
            .max_in_flight(max)
            .queue_capacity(8)
            .shed_policy(OverflowPolicy::EvictOldest)
            .build(),
    );

    let active_held = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for thread_id in 0..8 {
        let n = Arc::clone(&node);
        let active = Arc::clone(&active_held);
        let max_obs = Arc::clone(&max_observed);

        handles.push(std::thread::spawn(move || {
            for i in 0..200u64 {
                let task_id = thread_id * 1000 + i;
                if let Ok(guard) = n.enter(task_id, task_id, || {}) {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_obs.fetch_max(cur, Ordering::SeqCst);

                    // Micro-pause to maintain concurrency pressure
                    std::thread::yield_now();

                    active.fetch_sub(1, Ordering::SeqCst);
                    guard.commit();
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join");
    }

    // In-flight slots must never exceed max + 1
    let ceiling = max + 1;
    let peak = max_observed.load(Ordering::SeqCst);
    assert!(
        peak <= ceiling,
        "concurrent in-flight ({peak}) must never exceed hard clamp ceiling ({ceiling})"
    );

    // After all threads exit and commit, node must be completely idle
    assert_eq!(active_held.load(Ordering::SeqCst), 0);
    assert!(
        node.bulkhead_has_room(),
        "node must have full room after drain"
    );
    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "all lanes must return to zero"
    );
}
