//! Pattern 02: Worker Daemon & Autonomous Admission
//!
//! Demonstrates decoupling thread spawning from concurrency limits.
//! The spawning thread never holds the admission slot. Instead, the spawned
//! worker task claims its permit autonomously from within its own execution context.

use snubber::SnubberNode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct HardwareState {
    device_active: AtomicBool,
}

impl HardwareState {
    fn new() -> Self {
        Self {
            device_active: AtomicBool::new(false),
        }
    }
}

fn main() {
    println!("=== Pattern 02: Worker Daemon Lifecycle ===");

    // Limit maximum concurrent workers to exactly 1
    let node = Arc::new(
        SnubberNode::builder("worker_daemon")
            .max_in_flight(1)
            .build(),
    );

    let state = Arc::new(HardwareState::new());

    // Spawn long-lived worker
    let n = Arc::clone(&node);
    let s = Arc::clone(&state);

    let worker_handle = thread::spawn(move || {
        // The worker claims its slot internally.
        // If the thread dies or encounters a panic, the rollback closure executes.
        let s_cleanup = Arc::clone(&s);
        let guard = match n.probe_erased_auto(move || {
            println!("[Daemon Rollback] Worker exited or aborted. Disarming hardware...");
            s_cleanup.device_active.store(false, Ordering::SeqCst);
        }) {
            Ok(g) => g,
            Err(e) => {
                println!("[Worker] Failed to acquire admission slot: {:?}", e);
                return;
            }
        };

        // Activate device safely
        s.device_active.store(true, Ordering::SeqCst);
        println!("[Worker] Device active. Running high-frequency processing loop...");

        // Simulate work iterations
        for i in 1..=3 {
            thread::sleep(Duration::from_millis(30));
            println!("[Worker] Cycle {} complete", i);
        }

        // Clean completion
        guard.commit();
        s.device_active.store(false, Ordering::SeqCst);
        println!("[Worker] Loop finished cleanly.");
    });

    // Verify caller does not block and slots are properly accounted
    println!("[Main] Spawning complete. Main thread remains unblocked.");
    worker_handle.join().unwrap();

    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "All slots must be released upon worker termination"
    );
    assert!(
        !state.device_active.load(Ordering::SeqCst),
        "Hardware must be clean"
    );
    println!("Verification: In-flight count strictly 0. Daemon lifecycle verified.");
}
