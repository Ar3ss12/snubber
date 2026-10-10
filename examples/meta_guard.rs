//! Meta-guard: the dumb watches the smart (defense in depth).
//!
//! A zero-level `SnubberGuard` (core, no lock, no alloc) wraps a full
//! `SnubberNode` session. Clean path disarms both; panic inside — the
//! outer guard still drops the hardware. Direction is ONE-WAY ONLY:
//! dumb guards smart, never smart guards dumb (no node-in-node).
//!
//! Run: `cargo run --example meta_guard`

use snubber::{SnubberGuard, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};

static RELEASES: AtomicU64 = AtomicU64::new(0);

fn release() {
    RELEASES.fetch_add(1, Ordering::SeqCst);
}

fn main() {
    let node = SnubberNode::desktop("meta/demo");

    // --- clean path: both guards disarmed ---
    RELEASES.store(0, Ordering::SeqCst);
    {
        let meta = SnubberGuard::new(0, 0, release);
        let app = node.enter(1, 1, release).expect("admit");
        // ... work ...
        app.commit();
        meta.commit();
    }
    assert_eq!(RELEASES.load(Ordering::SeqCst), 0, "clean: no rollback");

    // --- panic inside: meta still drops the hardware ---
    RELEASES.store(0, Ordering::SeqCst);
    let r = std::panic::catch_unwind(|| {
        let _meta = SnubberGuard::new(0, 0, release);
        let _app = node.enter(2, 2, release).expect("admit");
        panic!("work exploded");
        // both guards drop here: inner rollback + outer rollback
    });
    assert!(r.is_err(), "panic propagated");
    assert_eq!(
        RELEASES.load(Ordering::SeqCst),
        2,
        "both levels fired, hardware free (idempotent)"
    );
    println!("META_GUARD: dumb watches smart, 2 releases, 0 harm");
}
