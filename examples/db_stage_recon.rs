//! Example 1: Database transactions with Stage Checkpoints and Reconciliation.
//!
//! Demonstrates the DB Decision Matrix:
//! - Failure at TxBegin / Connecting: Safe to retry.
//! - Failure at InFlight / QueryExecuting: NOT safe to retry blindly (risk of double write);
//!   reconcile status or return Pending.
//! - BreakerOpen: Database is down; serve cached fallback via `fallback_value()`.

use snubber::{BreakerState, EnterError, SnubberNode};

#[derive(Debug, PartialEq, Eq)]
enum TxOutcome {
    Success,
    SafeRetryNeeded,
    ReconciliationPending,
    FallbackServed(u64),
}

fn execute_db_operation(
    node: &SnubberNode,
    epoch: u64,
    tx_id: u64,
    fail_at_stage: Option<u32>,
) -> Result<TxOutcome, &'static str> {
    // 1. Check if node is tripped
    match node.enter(epoch, tx_id, || {
        // Real rollback: clean up local lock or connection handle
    }) {
        Ok(mut guard) => {
            // Stage 1: Connect to pool / BEGIN transaction
            guard.checkpoint(1, "tx_begin");
            if fail_at_stage == Some(1) {
                // Connection dropped at BEGIN -> safe retry
                let snap = guard.state_snapshot();
                eprintln!(
                    "[DB] Failed at stage {} ({}): safe retry allowed",
                    snap.stage,
                    snap.stage_label.unwrap_or("")
                );
                // Drop without commit triggers rollback & black-box recording
                return Ok(TxOutcome::SafeRetryNeeded);
            }

            // Stage 2: Execute mutating query (e.g. UPDATE balance)
            guard.checkpoint(2, "executing_update");
            if fail_at_stage == Some(2) {
                // Timeout / network lost while waiting for SQL response
                let snap = guard.state_snapshot();
                eprintln!(
                    "[DB] Failed at stage {} ({}): DO NOT RETRY BLINDLY! Enqueue for reconciliation.",
                    snap.stage,
                    snap.stage_label.unwrap_or("")
                );
                return Ok(TxOutcome::ReconciliationPending);
            }

            // Stage 3: COMMIT transaction
            guard.checkpoint(3, "committed");
            guard.commit();
            Ok(TxOutcome::Success)
        }
        Err(EnterError::BreakerOpen) => {
            // DB is isolated; serve fallback
            let fallback = node.fallback_value().unwrap_or(0);
            Ok(TxOutcome::FallbackServed(fallback))
        }
        Err(_) => Err("rejected by bulkhead or rate limit"),
    }
}

fn main() {
    println!("=== Snubber DB Decision Matrix Example ===");

    let node = SnubberNode::builder("postgres_cluster")
        .max_in_flight(10)
        .trip_after_failures(2)
        .fallback(|| 42) // last known cached balance/value
        .build();

    // Case A: Safe retry at stage 1
    let outcome1 = execute_db_operation(&node, 1, 101, Some(1)).unwrap();
    assert_eq!(outcome1, TxOutcome::SafeRetryNeeded);

    // Case B: In-flight timeout at stage 2 -> Must reconcile, not retry!
    let outcome2 = execute_db_operation(&node, 2, 102, Some(2)).unwrap();
    assert_eq!(outcome2, TxOutcome::ReconciliationPending);

    // After 2 failures, breaker trips
    let stats = node.stats();
    assert_eq!(stats.implicit_drops, 2);
    assert_eq!(stats.breaker_state, BreakerState::HalfOpen);

    // Case C: Success path
    let outcome3 = execute_db_operation(&node, 3, 103, None).unwrap();
    assert_eq!(outcome3, TxOutcome::Success);
    println!("Postgres transaction flow verified successfully!");
}
