//! Pattern 03: Database Bulkhead & Async Backpressure
//!
//! Demonstrates protecting downstream resources (databases, network sockets, microservices)
//! from sudden load surges using an admission bulkhead and backpressure pacing.
//! When capacity is exceeded, callers receive actionable backpressure metadata (`retry_after_ms`).

use snubber::{EnterError, SnubberNode};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn simulate_database_query(node: &Arc<SnubberNode>, client_id: usize) -> Result<(), &'static str> {
    // Attempt admission with max concurrency enforced
    let guard = match node.enter_erased_auto(move || {
        println!(
            "[Rollback] Query failed mid-flight for client {}",
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

    // Execute simulated protected database query
    println!(
        "[DB] Client {} query admitted. In-flight lanes: {:?}",
        client_id,
        node.lane_in_flight()
    );
    thread::sleep(Duration::from_millis(40));

    // Commit upon successful execution
    guard.commit();
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
            let res = simulate_database_query(&n, client_id);
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
    println!("\nVerification: Downstream database protected from unbounded connection exhaustion.");
}
