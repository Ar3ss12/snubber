//! Unwind safety scenario: "Panic mid-execution".
//!
//! Resources acquired, operation in-flight, then an unexpected panic occurs.
//! Stack unwinding must trigger the rollback exactly once and leave resources clean.
//!
//! Run: `cargo run --example unwind_safety`

use snubber::SnubberGuard;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

fn main() {
    println!("[UNWIND:START] scenario=panic_recovery");
    static HW: AtomicBool = AtomicBool::new(false);
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);

    let probe = AssertUnwindSafe(|| {
        let _guard = SnubberGuard::new(99, 31337, || {
            HW.store(false, Ordering::SeqCst);
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        });
        HW.store(true, Ordering::SeqCst);
        // ... 99% of the work happens here ...
        panic!("simulated_task_panic");
    });
    let caught = catch_unwind(probe).expect_err("must unwind");
    let msg = caught
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| caught.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic>");
    assert!(
        msg.contains("simulated_task_panic"),
        "wrong panic payload: {msg}"
    );
    assert!(
        !HW.load(Ordering::SeqCst),
        "hardware retained active state after unwind"
    );
    let rollbacks = ROLLBACKS.load(Ordering::SeqCst);
    assert_eq!(
        rollbacks, 1,
        "rollback must run exactly once, not zero, not twice"
    );
    println!("[UNWIND:RECOVERY] status=clean caught=true rollbacks={rollbacks}");
    println!("[TEST:RESULT] scenario=unwind_safety status=passed");
}
