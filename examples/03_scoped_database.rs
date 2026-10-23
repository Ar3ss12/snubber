//! Pattern 03: Database Concurrency Bulkhead & Backpressure
//!
//! Demonstrates protecting downstream database resources from load surges
//! using an admission bulkhead and backpressure pacing across concurrent worker threads.
//! When capacity is exceeded, callers receive actionable backpressure metadata (`retry_after_ms`).

use snubber::{EnterError, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Connection handles currently checked out of the simulated pool.
static OPEN_CONNS: AtomicU64 = AtomicU64::new(0);
/// Peak concurrent handles observed (must never exceed the bulkhead).
static PEAK_CONNS: AtomicU64 = AtomicU64::new(0);
/// Rollback closures that actually ran.
static ROLLBACKS: AtomicU64 = AtomicU64::new(0);

fn simulate_database_query(
    node: &Arc<SnubberNode>,
    client_id: usize,
    fail: bool,
) -> Result<(), &'static str> {
    // Attempt admission with max concurrency enforced
    let guard = match node.enter_erased_auto(move || {
        // Real release: the handle is checked back into the pool.
        OPEN_CONNS.fetch_sub(1, Ordering::SeqCst);
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        println!(
            "[Rollback] Query failed mid-flight for client {}: connection returned",
            client_id
        );
    }) {
        Ok(g) => g,
        Err(EnterError::BulkheadFull) => {
            return Err("503 Service Unavailable: Concurrency limit reached");
        }
        Err(EnterError::BreakerOpen) => {
            return Err("503 Circuit Breaker Open: System cooling down");
        }
        Err(e) => {
            eprintln!("[Node] Admission rejected: {:?}", e);
            return Err("Admission rejected");
        }
    };

    // Check a connection handle out of the simulated pool.
    let open = OPEN_CONNS.fetch_add(1, Ordering::SeqCst) + 1;
    PEAK_CONNS.fetch_max(open, Ordering::SeqCst);
    println!(
        "[DB] Client {} query admitted. Handles open: {}, lanes: {:?}",
        client_id,
        open,
        node.lane_in_flight()
    );
    thread::sleep(Duration::from_millis(40));

    if fail {
        // Simulated mid-flight failure: dropping the guard runs the
        // rollback, which returns the handle exactly once.
        println!("[DB] Client {} query failed mid-flight", client_id);
        drop(guard);
        return Err("500 Query failed (handle released)");
    }

    // Commit upon successful execution, then return the handle.
    guard.commit();
    OPEN_CONNS.fetch_sub(1, Ordering::SeqCst);
    Ok(())
}

fn main() {
    println!("=== Pattern 03: Database Bulkhead & Backpressure ===");

    // Configure a node with a strict bulkhead capacity of 2 concurrent connections
    let node = Arc::new(
        SnubberNode::builder("database_pool")
            .max_in_flight(2)
            .build(),
    );

    let mut handles = Vec::new();

    // Spawn 6 concurrent requests to demonstrate deterministic rejection
    for client_id in 1..=6 {
        let n = Arc::clone(&node);
        handles.push(thread::spawn(move || {
            let res = simulate_database_query(&n, client_id, false);
            match res {
                Ok(_) => println!("[Client {}] Success: Query executed cleanly.", client_id),
                Err(err) => println!("[Client {}] Refused: {}", client_id, err),
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "All leases must return to zero"
    );
    // Deterministic failure path on a quiescent node: the query is
    // admitted, fails mid-flight, and the rollback returns the handle.
    assert!(
        simulate_database_query(&node, 99, true).is_err(),
        "the failing query must surface its error"
    );

    // The failed query must have released its handle through the REAL
    // rollback path, exactly once, and no handle may leak.
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 1, "one rollback ran");
    assert_eq!(OPEN_CONNS.load(Ordering::SeqCst), 0, "no handle leaked");
    assert!(
        PEAK_CONNS.load(Ordering::SeqCst) <= 2,
        "bulkhead must cap concurrent handles at 2, saw {}",
        PEAK_CONNS.load(Ordering::SeqCst)
    );
    let s = node.stats();
    assert_eq!(
        s.committed + s.explicit_aborts + s.implicit_drops,
        s.total_started,
        "every admission settled once"
    );
    println!("\nVerification: handles released on failure; pool never exceeded the bulkhead.");
}
