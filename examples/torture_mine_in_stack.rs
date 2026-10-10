//! Torture scenario 3: "Mine in the stack".
//!
//! Resources clamped, 99% of the way done, then `panic!`. Unwind must run
//! the rollback exactly once and leave the hardware clean — and a rollback
//! that panics itself would abort the process, so it must be plain stores.
//!
//! Run: `cargo run --example torture_mine_in_stack`

use snubber::SnubberGuard;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

fn main() {
    println!("--- scenario 3: mine in the stack (panic at 99%) ---");
    static HW: AtomicBool = AtomicBool::new(false);
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);

    let probe = AssertUnwindSafe(|| {
        let _guard = SnubberGuard::new(99, 31337, || {
            HW.store(false, Ordering::SeqCst);
            ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        });
        HW.store(true, Ordering::SeqCst);
        // ... 99% of the work happens here ...
        panic!("remote detonation");
    });
    let caught = catch_unwind(probe).expect_err("must unwind");
    let msg = caught
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| caught.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic>");
    assert!(
        msg.contains("remote detonation"),
        "wrong panic payload: {msg}"
    );
    assert!(!HW.load(Ordering::SeqCst), "hardware STUCK after unwind");
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        1,
        "rollback must run exactly once, not zero, not twice"
    );
    println!("ok: panic caught, rollback ran exactly once, hardware clean");
    println!("SCENARIO 3 PASSED");
}
