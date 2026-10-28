//! Drain-backed liveness sample and the shared post-mortem rescue rule.
//!
//! `PipeDrain::sentinel_sample` turns pipe state into one observation, and
//! `await_post_mortem` is the single EOF-grace rule both `run_loop` and an
//! application-side supervisor use. These tests pin the part a missing
//! grace loses: a token that arrives after the child died but before the
//! pipe closed.

use std::time::{Duration, Instant};

use snubber::{await_post_mortem, PipeDrain, ProcessToken};

#[test]
fn armed_tracks_arm_and_commit_without_consuming() {
    let drain = PipeDrain::manual();
    assert!(!drain.armed(), "nothing armed before the first frame");

    assert_eq!(drain.feed(&ProcessToken::simple(7, 5, 1).to_bytes()), 1);
    assert!(drain.armed(), "a non-commit token stays armed");
    assert!(drain.armed(), "armed() is a peek, not a take");

    assert_eq!(drain.feed(&ProcessToken::commit(7).to_bytes()), 1);
    assert!(!drain.armed(), "a commit settles the armed token");

    assert_eq!(drain.counters().arms, 1);
    assert_eq!(drain.counters().commits, 1);
}

#[test]
fn sentinel_sample_reports_pipe_state_and_no_witness() {
    let drain = PipeDrain::manual();
    let sample = drain.sentinel_sample();
    assert_eq!(sample.idle_ms, 0, "no frame ever seen: idle is 0, not age");
    assert!(!sample.armed);
    assert_eq!(
        sample.witness_alive, None,
        "the drain sees traffic, not the process table"
    );

    drain.feed(&ProcessToken::simple(9, 5, 2).to_bytes());
    let sample = drain.sentinel_sample();
    assert!(sample.armed, "sample follows the armed slot");
    assert_eq!(sample.idle_ms, 0, "a frame just arrived");
}

#[test]
fn late_token_inside_grace_is_rescued() {
    let drain = PipeDrain::manual();
    let writer = drain.clone();
    std::thread::spawn(move || {
        // The child died, but its last frame is still in flight.
        std::thread::sleep(Duration::from_millis(15));
        writer.feed(&ProcessToken::simple(42, 5, 3).to_bytes());
        writer.finish();
    });

    let token = await_post_mortem(&drain, 250).expect("late token must be rescued");
    assert_eq!(token.task, 42);
    assert_eq!(token.opcode, 5);
}

#[test]
fn expired_grace_without_token_returns_none() {
    let drain = PipeDrain::manual();
    let started = Instant::now();
    assert!(
        await_post_mortem(&drain, 20).is_none(),
        "no token armed: a clean exit"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(20),
        "the helper must wait out the grace, not sample once"
    );
}

#[test]
fn armed_token_survives_grace_expiry_for_the_caller() {
    let drain = PipeDrain::manual();
    drain.feed(&ProcessToken::simple(5, 6, 1).to_bytes());

    let token = await_post_mortem(&drain, 5).expect("an armed token is returned after EOF grace");
    assert_eq!(token.task, 5);
    assert!(!drain.armed(), "the rescue consumed the slot exactly once");
}
