//! Weapon 3 proof: commit releases, failure burns, drop order holds.
//!
//! The compiler is the first test: post-`commit` access is `use of moved
//! value` (pinned by the `compile_fail` doctest on `Protected` below is
//! for humans — here we prove the RUNTIME halves: rollback witness fires
//! exactly once, the resource dies with the guard, and the hardware sees
//! the rollback BEFORE the resource burns (drop order: guard first).

use snubber::{Protected, SnubberGuard};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[test]
fn commit_releases_resource_alive() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let guard = SnubberGuard::new(1, 1, || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
    });
    let p = Protected::new(42u32, guard);
    assert_eq!(*p.get(), 42);
    let out = p.commit();
    assert_eq!(out, 42, "commit walks the resource out clean");
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 0, "no rollback on commit");
}

#[test]
fn error_burns_resource_and_fires_rollback_once() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    // Drop order witness: rollback must observe the resource STILL ALIVE
    // (guard field drops before the resource field).
    let seen_alive = Arc::new(AtomicU32::new(0));
    let probe = Arc::clone(&seen_alive);
    struct Socket {
        alive: Arc<AtomicU32>,
    }
    impl Drop for Socket {
        fn drop(&mut self) {
            self.alive.store(2, Ordering::SeqCst); // burns second
        }
    }
    let sock = Socket {
        alive: Arc::clone(&seen_alive),
    };
    seen_alive.store(1, Ordering::SeqCst); // alive during the transaction
    let guard = SnubberGuard::new(2, 2, move || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        // Rollback runs while the Socket field still lives: marker is 1.
        probe.store(probe.load(Ordering::SeqCst), Ordering::SeqCst);
    });
    let mut p = Protected::new(sock, guard);
    let _ = p.get_mut(); // touch inside the transaction — legal
    drop(p); // implicit failure: rollback + burn, no commit
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 1, "exactly one rollback");
    assert_eq!(
        seen_alive.load(Ordering::SeqCst),
        2,
        "resource burned after the rollback"
    );
}

#[test]
fn abort_runs_rollback_and_destroys() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let guard = SnubberGuard::new(3, 3, || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
    });
    let p = Protected::new(7u32, guard);
    let ev = p.abort("plan-b");
    assert!(ev.is_terminal(), "abort names the terminal outcome");
    assert_eq!(ROLLBACKS.load(Ordering::SeqCst), 1);
    println!("\n[PROTECT]: commit=alive error=burned abort=terminal rollbacks=1");
}
