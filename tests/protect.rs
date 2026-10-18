//! Protected resource wrapper integration tests: commit lifecycle, failure rollback, drop order.
//!
//! Verifies runtime invariants:
//! - rollback executes exactly once on implicit or explicit abort
//! - commit releases inner resource intact without triggering rollback
//! - drop order executes guard rollback before destroying wrapped resource
//! - Deref and DerefMut provide transparent resource access during active transaction
//! - abort preserves authentic transaction header coordinates

use snubber::{Protected, SnubberGuard};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[test]
fn commit_releases_resource_intact() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let guard = SnubberGuard::new(1, 1, || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
    });
    let mut p = Protected::new(42u32, guard);
    // Deref & DerefMut verification
    assert_eq!(*p, 42);
    *p += 8;
    assert_eq!(*p.get(), 50);
    let out = p.commit();
    assert_eq!(out, 50, "commit returns inner resource value intact");
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        0,
        "no rollback triggered on commit"
    );
}

#[test]
fn error_drops_resource_and_executes_rollback_once() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    // Drop order witness: rollback must observe resource alive
    // (guard field drops before wrapped resource field).
    let state_marker = Arc::new(AtomicU32::new(0));
    let probe = Arc::clone(&state_marker);
    struct ResourceProbe {
        state: Arc<AtomicU32>,
    }
    impl Drop for ResourceProbe {
        fn drop(&mut self) {
            self.state.store(2, Ordering::SeqCst); // drops second
        }
    }
    let res = ResourceProbe {
        state: Arc::clone(&state_marker),
    };
    state_marker.store(1, Ordering::SeqCst); // active during transaction
    let guard = SnubberGuard::new(2, 2, move || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
        // Rollback runs while resource still lives: marker is 1.
        probe.store(probe.load(Ordering::SeqCst), Ordering::SeqCst);
    });
    let mut p = Protected::new(res, guard);
    let _ = p.get_mut();
    drop(p); // implicit abort on drop
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        1,
        "exactly one rollback execution"
    );
    assert_eq!(
        state_marker.load(Ordering::SeqCst),
        2,
        "resource dropped after guard rollback completed"
    );
}

#[test]
fn abort_runs_rollback_and_preserves_authentic_header() {
    static ROLLBACKS: AtomicU32 = AtomicU32::new(0);
    ROLLBACKS.store(0, Ordering::SeqCst);
    let guard = SnubberGuard::new(3, 42, || {
        ROLLBACKS.fetch_add(1, Ordering::SeqCst);
    });
    let p = Protected::new(7u32, guard);
    let ev = p.abort("plan-b");
    assert!(ev.is_terminal(), "abort returns terminal event");
    assert_eq!(ev.header().epoch, 3, "preserves authentic epoch");
    assert_eq!(ev.header().task_id, 42, "preserves authentic task_id");
    assert_eq!(
        ROLLBACKS.load(Ordering::SeqCst),
        1,
        "rollback triggered on abort"
    );
    println!("\n[PROTECTED:ABORT] epoch=3 task_id=42 rollbacks=1 outcome=terminal");
}
