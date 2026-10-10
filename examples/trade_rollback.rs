//! Game ECS trade: sword leaves chest → must land in backpack, or return.
//! No dup, no void. Two-step transaction with compensating rollback.
//!
//! Run: `cargo run --example trade_rollback`

use snubber::SnubberNode;
use std::sync::atomic::{AtomicBool, Ordering};

static IN_CHEST: AtomicBool = AtomicBool::new(true);
static IN_PACK: AtomicBool = AtomicBool::new(false);

fn main() {
    let node = SnubberNode::server("game/trade");
    let guard = node
        .enter(1, 1, || {
            // compensating transaction: sword returns to chest
            IN_PACK.store(false, Ordering::SeqCst);
            IN_CHEST.store(true, Ordering::SeqCst);
        })
        .expect("admit");
    // Step 1: remove from chest.
    IN_CHEST.store(false, Ordering::SeqCst);
    // Step 2 fails: inventory full / disconnect → NO commit.
    drop(guard);
    assert!(IN_CHEST.load(Ordering::SeqCst), "sword back in chest");
    assert!(!IN_PACK.load(Ordering::SeqCst), "no dup in pack");
    println!("TRADE_ROLLBACK: no dup, no void");
}
