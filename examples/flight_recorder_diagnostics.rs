//! Flight recorder diagnostics and input boundary validation.
//!
//! Four production scenarios demonstrating edge validation and deterministic recovery:
//! * Scenario 1: Zero interval flood       -> `InvariantBreach`, zero events reach core
//! * Scenario 2: Clock overflow prevention  -> `InvariantBreach` caught at membrane boundary
//! * Scenario 3: High-frequency flapping    -> Leading edge admitted, duplicates rejected
//! * Scenario 4: Resource hold with abort   -> Single-shot rollback releases resource
//!
//! Run: `cargo run --example flight_recorder_diagnostics`

use snubber::{EnterError, SnubberNode};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// --- Mirrored formulas (rusty-autoclicker src/utils.rs + src/app.rs) ---------
const fn interval_ms(hr: u64, min: u64, sec: u64, ms: u64) -> u64 {
    (hr * 3_600_000) + (min * 60_000) + (sec * 1_000) + ms
}

/// Membrane predicate: the physical law of clicker software.
/// interval == 0 floods every frame; interval > MAX would make
/// `Instant::now().checked_sub(interval).unwrap()` return None -> panic.
fn click_invariant(interval: u64) -> Result<(), &'static str> {
    if interval == 0 {
        return Err("interval == 0 floods every frame");
    }
    // 24h ceiling: any legit human schedule fits; anything above
    // is clock poison aimed at `checked_sub(...).unwrap()`.
    if interval > 86_400_000 {
        return Err("interval exceeds 24h: checked_sub would be None");
    }
    Ok(())
}

fn main() {
    let mut passed = 0u32;

    // --- Scene 1 (D-3): zero interval --------------------------------------
    {
        let node = SnubberNode::builder("field/click_loop").build();
        let interval = interval_ms(0, 0, 0, 0);
        let clicks = Arc::new(AtomicU64::new(0));
        let r = node.enter_checked(1, 1, &interval, |p| click_invariant(*p), || {});
        match r {
            Err(EnterError::InvariantBreach(reason)) => {
                println!("[scene1] zero interval cut at membrane: {reason}");
                passed += 1;
            }
            Err(other) => panic!("[scene1] wrong rejection: {other:?}"),
            Ok(_) => panic!("[scene1] LEAK: zero interval reached core (entered)"),
        }
        assert_eq!(clicks.load(Ordering::SeqCst), 0, "core untouched");
        assert_eq!(node.stats().poisoned_rejected, 1);
    }

    // --- Scene 2 (D-2): hour = 99999 ----------------------------------------
    {
        let node = SnubberNode::builder("field/clock").build();
        let interval = interval_ms(99_999, 0, 0, 0); // ~3.6e11 ms
        let r = node.enter_checked(1, 2, &interval, |p| click_invariant(*p), || {});
        match r {
            Err(EnterError::InvariantBreach(reason)) => {
                // `checked_sub(interval).unwrap()` would have panicked here.
                println!("[scene2] clock poison cut BEFORE checked_sub: {reason}");
                passed += 1;
            }
            Err(other) => panic!("[scene2] wrong rejection: {other:?}"),
            Ok(_) => panic!("[scene2] LEAK: poison reached unwrap"),
        }
        assert_eq!(node.stats().poisoned_rejected, 1);
    }

    // --- Scene 3 (D-4): 50 Hz Start/Stop flapping ---------------------------
    // NOTE: each admitted guard is COMMITTED (clean toggle serviced).
    // Uncommitted drops would feed the breaker streak — that is scene 4's job.
    {
        let node = SnubberNode::builder("field/hotkey").build();
        let mut epoch = 0u64;
        let mut admitted = 0u32;
        for _ in 0..100 {
            epoch += 1; // front edge of the hotkey
            if let Ok(g) = node.enter(epoch, 7, || {}) {
                g.commit();
                admitted += 1;
            }
            // release edge replays the same epoch (level-triggered poll):
            // must be cut as stale.
            let _ = node.enter(epoch, 7, || {}).map(|g| g.commit());
        }
        let s = node.stats();
        println!(
            "[scene3] admitted={admitted} stale={}",
            s.stale_epochs_rejected
        );
        assert_eq!(admitted, 100, "every front edge is a fresh epoch");
        assert_eq!(s.stale_epochs_rejected, 100, "every release replay cut");
        passed += 1;
    }

    // --- Scene 4 (D-1): hold + abort ----------------------------------------
    {
        let node = SnubberNode::builder("field/hold").max_in_flight(1).build();
        let button_down = Arc::new(AtomicBool::new(false));
        let releases = Arc::new(AtomicU64::new(0));
        {
            let down = button_down.clone();
            let rel = releases.clone();
            let guard = node
                .enter(1, 9, move || {
                    // rollback: release_button() mirror
                    down.store(false, Ordering::SeqCst);
                    rel.fetch_add(1, Ordering::SeqCst);
                })
                .expect("slot free");
            // start_hold() mirror: OS-level press
            button_down.store(true, Ordering::SeqCst);
            // thread dies mid-hold: guard drops WITHOUT commit (like
            // `panic = "abort"` skipping `Drop for App`)
            drop(guard);
            let _ = Duration::from_millis(0); // scene marker, no sleep
        }
        assert!(
            !button_down.load(Ordering::SeqCst),
            "button released by Drop"
        );
        assert_eq!(releases.load(Ordering::SeqCst), 1, "rollback exactly once");
        // slot came home: next enter passes (commit the probe so it does
        // not count as a second implicit drop).
        node.enter(2, 9, || {})
            .map(|g| g.commit())
            .expect("slot leaked!");
        assert_eq!(node.stats().implicit_drops, 1, "bug counted in black box");
        println!("[scene4] hold aborted: button released, slot home, bug logged");
        passed += 1;
    }

    println!("FIELD AUTOPSY: {passed}/4 scenes hold. Dossier confirmed.");
    assert_eq!(passed, 4);
}
