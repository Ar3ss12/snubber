//! Stream ordering scenario: "Out-of-order dispatcher".
//!
//! 10 threads fire epochs 1..=50 through one channel with random jitter.
//! The compiler sees valid types + Send; the runtime delivers shuffled ordering.
//! [`snubber::EpochGate`] admits strictly the increasing subsequence — no stale frames.
//!
//! Run: `cargo run --example out_of_order_stream`

#[path = "common/mod.rs"]
mod common;

use common::Lcg;
use snubber::{EpochGate, TransactionHeader};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn main() {
    println!("--- scenario 1: drunk dispatcher (shuffled epochs 1..=50) ---");
    let (tx, rx) = mpsc::channel::<TransactionHeader>();

    let mut rng = Lcg(0xC0FFEE);
    let delays: Vec<u64> = (0..50).map(|_| rng.next() % 7).collect();

    let mut handles = Vec::new();
    for (i, d) in delays.into_iter().enumerate() {
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            thread::sleep(Duration::from_micros(d * 50));
            let h = TransactionHeader::new((i + 1) as u64, 1);
            tx.send(h).expect("receiver alive during burst");
        }));
    }
    drop(tx); // last sender goes away once clones finish

    let mut gate = EpochGate::new();
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut last: u64 = 0;
    for h in rx {
        if gate.observe(&h) {
            // Fresh: must be strictly ahead of everything accepted before.
            assert!(
                h.epoch > last,
                "gate let a stale epoch through: {h:?} after {last}"
            );
            last = h.epoch;
            accepted += 1;
        } else {
            assert!(
                h.epoch <= gate.last_seen(),
                "rejected packet {h:?} looks fresh vs gate {}",
                gate.last_seen()
            );
            rejected += 1;
        }
    }
    for h in handles {
        h.join().expect("sender thread panicked");
    }

    // Epoch 50 exists, so the max must have been accepted exactly once;
    // everything else is either the increasing chain or rejected mush.
    assert_eq!(gate.last_seen(), 50, "epoch 50 must arrive and be accepted");
    assert!(accepted >= 1, "at least the max epoch must be accepted");
    assert_eq!(accepted + rejected, 50, "every packet accounted for");
    println!("ok: accepted={accepted} rejected={rejected} last_seen=50");
    println!("SCENARIO 1 PASSED");
}
