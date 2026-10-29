//! Pattern 20: Rollback Budget — name a wedged rollback from outside.
//!
//! The rollback body re-takes a mutex that the work closure already holds on
//! the same thread, so the body never returns. The crate cannot cancel it
//! (`#![forbid(unsafe_code)]`) and does not kill caller threads; what it can
//! do is say so from another thread without the node lock.
//!
//! `snubber::watchdog::poll_rollback_node` reads the phase atomics and returns
//! `RollbackStall` once the body outlives the budget armed on the node —
//! the same number the builder took, so the observer cannot drift from it.
//! This process finishes in under two seconds even though the worker thread
//! never returns: `main` reports the verdict and exits.
//!
//! Run with:
//! ```powershell
//! cargo run --example 20_rollback_budget
//! ```

use snubber::watchdog::poll_rollback_node;
use snubber::SnubberNode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BUDGET_MS: u64 = 200;
const GIVE_UP: Duration = Duration::from_millis(1_500);

fn main() {
    let node = SnubberNode::builder("examples/rollback-budget")
        .max_in_flight(1)
        .rollback_budget_ms(BUDGET_MS) // arms the phase clock
        .build();
    let lock = Arc::new(Mutex::new(()));

    let worker = {
        let node = node.clone();
        let lock = Arc::clone(&lock);
        std::thread::spawn(move || {
            // Held across the whole scope on THIS thread.
            let _held = lock.lock().expect("work holds the lock");
            let rollback_lock = Arc::clone(&lock);
            let _ = node.scoped::<i32, &'static str, _, _>(
                move || {
                    // Same thread, same mutex: this waits forever.
                    let _wedged = rollback_lock.lock().expect("rollback re-locks");
                },
                || Err("work failed"),
            );
        })
    };

    // The observer is this thread: no lock, no allocation, no deadline that
    // the library enforces — it reports what it measured.
    let deadline = Instant::now() + GIVE_UP;
    loop {
        if let Some(stall) = poll_rollback_node(&node) {
            println!(
                "RollbackStall epoch={} task_id={} elapsed_ms={} budget_ms={}",
                stall.epoch, stall.task_id, stall.elapsed_ms, stall.budget_ms
            );
            println!("the body never returned; main leaves it wedged and exits");
            break;
        }
        if Instant::now() >= deadline {
            eprintln!("no RollbackStall within {GIVE_UP:?} — the observer is broken");
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // A wedged thread cannot be joined. Dropping the handle detaches it and
    // the process ends with it — the crate never kills another thread.
    drop(worker);
}
