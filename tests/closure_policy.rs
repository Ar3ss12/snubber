//! Closure-capturing commands (`move || …`) — `alloc` feature.
//!
//! Run: `cargo test --features alloc --test closure_policy`

#![cfg(feature = "alloc")]

use snubber::ClosurePolicy;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[test]
fn closure_captures_context() {
    let attempts = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&attempts);
    let policy = ClosurePolicy::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    });
    policy.run();
    policy.run();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn closure_debug_does_not_leak_captures() {
    let x = 42u32;
    let policy = ClosurePolicy::new(move || {
        let _ = x;
    });
    assert_eq!(format!("{:?}", policy), "ClosurePolicy(..)");
}
