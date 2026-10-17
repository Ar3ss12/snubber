//! Field regression: postmortem regression scenarios as permanent tests.
//! Investigation only — the foreign repo is untouched; formulas mirrored.

use snubber::{EnterError, SnubberNode};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const fn interval_ms(hr: u64, min: u64, sec: u64, ms: u64) -> u64 {
    (hr * 3_600_000) + (min * 60_000) + (sec * 1_000) + ms
}

fn click_invariant(interval: u64) -> Result<(), &'static str> {
    if interval == 0 {
        return Err("interval == 0 floods every frame");
    }
    if interval > 86_400_000 {
        return Err("interval exceeds 24h: checked_sub would be None");
    }
    Ok(())
}

#[test]
fn zero_interval_never_reaches_core() {
    let node = SnubberNode::builder("field/click_loop").build();
    let interval = interval_ms(0, 0, 0, 0);
    match node.enter_checked(1, 1, &interval, |p| click_invariant(*p), || {}) {
        Err(EnterError::InvariantBreach(_)) => {}
        Err(other) => panic!("wrong rejection: {other:?}"),
        Ok(_) => panic!("LEAK: zero interval reached core"),
    }
    assert_eq!(node.stats().total_started, 0, "no slot consumed");
    assert_eq!(node.stats().poisoned_rejected, 1);
}

#[test]
fn hour_99999_cut_before_checked_sub() {
    let node = SnubberNode::builder("field/clock").build();
    let interval = interval_ms(99_999, 0, 0, 0);
    match node.enter_checked(1, 2, &interval, |p| click_invariant(*p), || {}) {
        Err(EnterError::InvariantBreach(_)) => {}
        Err(other) => panic!("wrong rejection: {other:?}"),
        Ok(_) => panic!("LEAK: poison reached unwrap"),
    }
    assert_eq!(node.stats().poisoned_rejected, 1);
}

#[test]
fn hotkey_flap_front_passes_replay_cut() {
    let node = SnubberNode::builder("field/hotkey").build();
    let mut admitted = 0u32;
    for epoch in 1..=100u64 {
        if let Ok(g) = node.enter(epoch, 7, || {}) {
            g.commit();
            admitted += 1;
        }
        let _ = node.enter(epoch, 7, || {}).map(|g| g.commit());
    }
    assert_eq!(admitted, 100);
    assert_eq!(node.stats().stale_epochs_rejected, 100);
}

#[test]
fn hold_abort_releases_button_and_slot() {
    let node = SnubberNode::builder("field/hold").max_in_flight(1).build();
    let button_down = Arc::new(AtomicBool::new(false));
    let releases = Arc::new(AtomicU64::new(0));
    {
        let down = button_down.clone();
        let rel = releases.clone();
        let guard = node
            .enter(1, 9, move || {
                down.store(false, Ordering::SeqCst);
                rel.fetch_add(1, Ordering::SeqCst);
            })
            .expect("slot free");
        button_down.store(true, Ordering::SeqCst);
        drop(guard); // thread dies mid-hold, no commit
    }
    assert!(!button_down.load(Ordering::SeqCst));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
    node.enter(2, 9, || {})
        .map(|g| g.commit())
        .expect("slot leaked!");
    assert_eq!(node.stats().implicit_drops, 1);
}
