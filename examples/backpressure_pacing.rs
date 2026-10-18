//! Backpressure pacing: 100 tasks/s on a single-slot bulkhead.
//! Without pacing, excess tasks bounce with BulkheadFull; with `retry_after_ms` pacing
//! every task lands. AdviseOnly mode — the caller owns the wait.
//!
//! Run: `cargo run --example backpressure_pacing`

use snubber::SnubberNode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn main() {
    let node = SnubberNode::builder("pace/demo").max_in_flight(1).build();
    // Occupy the single slot in the background: the wall is UP.
    let _held = node.enter(1, 1, || {}).expect("slot held");
    let refused = Arc::new(AtomicU64::new(0));
    // Dumb hammer: fire 20 tasks with no pacing — all bounce off.
    for epoch in 2..=21u64 {
        match node.enter(epoch, epoch, || {}) {
            Ok(g) => {
                g.commit();
            }
            Err(e) => {
                refused.fetch_add(1, Ordering::SeqCst);
                let _hint = e.retry_after_ms(); // ignored by the hammer
            }
        }
    }
    let hammer_refused = refused.load(Ordering::SeqCst);
    println!("[hammer] refused={hammer_refused}/20 (wall up, hints ignored)");
    assert_eq!(hammer_refused, 20, "hammer must bounce");
    drop(_held);

    // Paced worker: holder releases after 30ms; worker paces on hints.
    let node2 = SnubberNode::builder("pace/worker").max_in_flight(1).build();
    let held2 = node2.enter(1, 1, || {}).expect("slot held");
    let n2 = node2.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        drop(held2); // wall comes down after 30ms
        let _ = &n2;
    });
    let mut landed = 0u32;
    let mut epoch = 100u64;
    let mut spins = 0u32;
    while landed < 1 && spins < 1000 {
        epoch += 1;
        spins += 1;
        match node2.enter(epoch, epoch, || {}) {
            Ok(g) => {
                g.commit();
                landed += 1;
            }
            Err(e) => {
                // 3rd law in the caller's hands: feel the wall, back off.
                let wait = e.retry_after_ms().clamp(1, 50);
                std::thread::sleep(std::time::Duration::from_millis(wait));
            }
        }
    }
    println!("[paced] landed={landed} after {spins} spins (hints obeyed)");
    assert_eq!(landed, 1, "pacing lands once the wall drops");
    println!("BACKPRESSURE_PACING: hammer bounces 20/20, pacing lands");
}
