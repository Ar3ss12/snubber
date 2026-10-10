//! Torture scenario 2: "Ruptured aorta".
//!
//! Backend clamps virtual hardware (atomic flag), then the UI drops its
//! `Receiver` (crashed WebView / pulled cable). `send` fails with
//! `SendError`, the `?` bails early — `Drop` must release the hardware.
//!
//! Run: `cargo run --example torture_ruptured_aorta`

use snubber::SnubberGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

fn main() {
    println!("--- scenario 2: ruptured aorta (receiver dropped mid-action) ---");
    static HW: AtomicBool = AtomicBool::new(false);

    fn backend(tx: mpsc::Sender<u32>) -> Result<(), mpsc::SendError<u32>> {
        let _guard = SnubberGuard::new(10, 77, || HW.store(false, Ordering::SeqCst));
        HW.store(true, Ordering::SeqCst); // clamp the hardware
        for step in 0..10u32 {
            tx.send(step)?; // dead channel -> SendError -> early return -> Drop
        }
        // Intentionally NOT committing: both paths below must release HW.
        Ok(())
    }

    let (tx, rx) = mpsc::channel::<u32>();
    drop(rx); // simulate the dead consumer BEFORE the backend runs
    let err = backend(tx).expect_err("send into a dropped channel must fail");
    assert_eq!(err.0, 0, "first send already hits the dead channel");
    assert!(
        !HW.load(Ordering::SeqCst),
        "hardware STUCK: Drop did not release it"
    );
    println!("ok: SendError bailed early, Drop released the hardware");
    println!("SCENARIO 2 PASSED");
}
