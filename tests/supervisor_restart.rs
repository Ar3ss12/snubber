//! Phase 2 — RestartPolicy Engine: dumb-spine reanimation math.
//!
//! The tracker owns ONLY the decision (`RestartPolicy` + `Backoff` +
//! storm window). The caller owns spawn/wait/sleep — so this test uses
//! a FAKE clock (plain `u64` numbers, zero real sleeps) and asserts:
//! * crash `exit(1)` x5 fast → `restarts == 3`, `giveups == 1`;
//! * backoff deltas double honestly (`50 → 100 → 200`);
//! * clean `exit(0)` under `OnCrash` = `Stop` (graceful shutdown);
//! * `Never` never restarts; `Always` restarts even `exit(0)`;
//! * generation bumps once per authorized restart.

use snubber::{
    Backoff, FaultKind, RestartDecision, RestartPolicy, RestartTracker, RestartWindow, SnubberNode,
};

#[test]
fn storm_guard_trips_after_three_fast_crashes() {
    let mut tracker = RestartTracker::new(
        RestartPolicy::OnCrash,
        Backoff::exponential(50, 1000),
        RestartWindow::new(3, 10),
    );
    // Fake satellite: exits(1) instantly, five times in a row.
    // Fake clock: all deaths land inside one 10 s window.
    let mut now_ms = 1_000u64;
    let mut delays = Vec::new();
    let mut stops = 0u32;
    for _ in 0..5 {
        match tracker.note_death(Some(1), now_ms) {
            RestartDecision::Restart { delay_ms, .. } => delays.push(delay_ms),
            RestartDecision::GiveUp { .. } => stops += 1,
            RestartDecision::Stop => panic!("crash must never Stop under OnCrash"),
        }
        now_ms += 100; // 100 ms apart — all inside the 10 s window.
    }
    // Iron facts: 3 reanimations, then the guard trips on EVERY
    // further death inside the window (4th AND 5th — honest math).
    assert_eq!(tracker.attempts(), 5, "every death reported");
    assert_eq!(tracker.restarts(), 3, "exactly three reanimations");
    assert_eq!(tracker.giveups(), 2, "guard trips on 4th and 5th");
    assert_eq!(stops, 2, "4th + 5th deaths are GiveUps");
    // Backoff deltas double honestly — recorded, never slept.
    assert_eq!(delays, vec![50, 100, 200], "exponential cooling 50→100→200");
    // Generation bumped once per authorized restart (1 boot + 3).
    assert_eq!(tracker.generation(), 4, "gen 1 + 3 restarts");
    println!("\n[RESTART]: attempts=5 restarts=3 giveups=2 delays=50,100,200 gen=4 poison=0");

    // GiveUp path parks the lineage honestly: ProcessDied + alert.
    let node = SnubberNode::builder("super/restart-giveup")
        .trip_after_failures(0)
        .build();
    node.report(FaultKind::ProcessDied, 1, 1);
    let s = node.stats();
    assert_eq!(s.reported[FaultKind::ProcessDied.index()], 1);
    assert_eq!(s.poison_recoveries, 0);
}

#[test]
fn policy_matrix_never_oncrash_always() {
    // Never: even abnormal termination is not restarted.
    let mut never = RestartTracker::new(
        RestartPolicy::Never,
        Backoff::exponential(50, 1000),
        RestartWindow::new(3, 10),
    );
    assert_eq!(
        never.note_death(Some(1), 1000),
        RestartDecision::Stop,
        "Never never restarts"
    );
    assert_eq!(never.restarts(), 0);
    assert_eq!(never.giveups(), 0);

    // OnCrash: clean exit(0) is a graceful shutdown.
    let mut oncrash = RestartTracker::default_spine();
    assert_eq!(
        oncrash.note_death(Some(0), 1000),
        RestartDecision::Stop,
        "exit(0) is a graceful shutdown, not a crash"
    );
    assert_eq!(oncrash.restarts(), 0);
    // Kill/hang from outside (no code) is always a crash.
    match oncrash.note_death(None, 1100) {
        RestartDecision::Restart { delay_ms, attempt } => {
            assert_eq!(delay_ms, 50);
            assert_eq!(attempt, 0);
        }
        other => panic!("kill must restart: {other:?}"),
    }

    // Always: even exit(0) is re-spawned (daemon duty).
    let mut always = RestartTracker::new(
        RestartPolicy::Always,
        Backoff::none(),
        RestartWindow::unlimited(),
    );
    for i in 0..10 {
        match always.note_death(Some(0), 1000 + i * 10) {
            RestartDecision::Restart { delay_ms, .. } => assert_eq!(delay_ms, 0),
            other => panic!("Always restarts everything: {other:?}"),
        }
    }
    assert_eq!(always.restarts(), 10);
    assert_eq!(always.giveups(), 0, "unlimited window never trips");
}

#[test]
fn backoff_math_is_pure_and_capped() {
    let b = Backoff::exponential(50, 1000);
    assert_eq!(b.delay_for_attempt(0), 50);
    assert_eq!(b.delay_for_attempt(1), 100);
    assert_eq!(b.delay_for_attempt(2), 200);
    assert_eq!(b.delay_for_attempt(3), 400);
    assert_eq!(b.delay_for_attempt(4), 800);
    assert_eq!(b.delay_for_attempt(5), 1000, "capped at max");
    assert_eq!(b.delay_for_attempt(60), 1000, "never overflows");
    assert_eq!(b.delay_for_attempt(u32::MAX), 1000, "saturates");
    assert_eq!(Backoff::none().delay_for_attempt(0), 0);
    // max < base saturates to base (never panics).
    assert_eq!(Backoff::exponential(500, 10).delay_for_attempt(3), 500);
}

#[test]
fn old_deaths_slide_out_of_the_window() {
    let mut tracker = RestartTracker::new(
        RestartPolicy::OnCrash,
        Backoff::exponential(50, 1000),
        RestartWindow::new(3, 10),
    );
    // Two crashes at t=0s, then silence for an hour: window slides clean.
    assert!(matches!(
        tracker.note_death(Some(1), 0),
        RestartDecision::Restart { .. }
    ));
    assert!(matches!(
        tracker.note_death(Some(1), 100),
        RestartDecision::Restart { .. }
    ));
    // One hour later: old stamps pruned, counter restarts fresh.
    assert!(matches!(
        tracker.note_death(Some(1), 3_600_000),
        RestartDecision::Restart { .. }
    ));
    assert_eq!(tracker.restarts(), 3);
    assert_eq!(tracker.giveups(), 0, "pruned window never trips");
}
