//! Pipeline Stress Scenario: Broken consumer channel.
//!
//! Backend clamps hardware resource, then the receiving end drops unexpectedly
//! (crashed worker / closed connection). `send` fails with `SendError`, the `?`
//! returns early — RAII `Drop` must guarantee the resource is safely released.
//!
//! Run: `cargo run --example heavy_pipeline_stress`

use snubber::SnubberGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

fn main() {
    println!("--- Pipeline Stress: broken channel recovery ---");
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
