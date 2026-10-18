//! Surgical trap proofs: one verdict per trap from the review, two tests each.
//!
//! * Trap 1 — no double-count of `ImplicitDrop` in `catastrophic_faults()`.
//! * Trap 2 — `#[track_caller]` coordinates land in record + ring slot.
//! * Trap 3 — `flight_depth(0)` clamps to 1, huge clamps to 64.
//! * Trap 4 — ring never grows past cap under flood / mixed paths.

use snubber::{FaultKind, SnubberNode};

#[test]
fn trap1_no_double_count_implicit_drop_vs_reported() {
    // Guard path (`settle_implicit`): ONE implicit drop bumps
    // `implicit_drops` exactly once. `catastrophic_faults()` sums
    // Panic/Timeout/ProcessDied reports + `implicit_drops` — the guard
    // path never touches `reported[ImplicitDrop]`, so one drop = one
    // fault, never two.
    let node = SnubberNode::builder("trap1_guard").build();
    let g = node.enter(1, 1, || {}).expect("admit");
    drop(g);
    let s = node.stats();
    assert_eq!(s.implicit_drops, 1);
    assert_eq!(s.reported[FaultKind::ImplicitDrop.index()], 0);
    assert_eq!(s.catastrophic_faults(), 1, "one drop = one fault, not two");
}

#[test]
fn trap1_report_implicit_drop_does_not_feed_streak_twice() {
    // Explicit `report(ImplicitDrop)` goes through `bump_reported`, so it
    // bumps BOTH `reported[]` and `implicit_drops` exactly once per call —
    // but it must NOT feed the breaker streak (settle owns the streak for
    // implicit drops). Two calls: counters move by 2, breaker stays Closed.
    let node = SnubberNode::builder("trap1_report")
        .trip_after_failures(3)
        .build();
    node.report(FaultKind::ImplicitDrop, 1, 1);
    node.report(FaultKind::ImplicitDrop, 2, 2);
    let s = node.stats();
    assert_eq!(s.reported[FaultKind::ImplicitDrop.index()], 2);
    assert_eq!(s.implicit_drops, 2, "one bump per call, never twice");
    assert_eq!(s.catastrophic_faults(), 2, "one incident = one fault");
    assert_eq!(
        s.breaker_state,
        snubber::BreakerState::Closed,
        "report(ImplicitDrop) must not feed the streak"
    );
    assert_eq!(s.poison_recoveries, 0);
}

#[test]
fn trap2_report_carries_caller_coordinates() {
    // The record must name THIS file, not `src/node.rs`.
    let node = SnubberNode::builder("trap2_report").build();
    node.report(FaultKind::Panic, 7, 7);
    let fault = node.stats().last_fault.expect("recorded");
    assert!(
        fault.file.ends_with("regression_postmortems.rs"),
        "caller coords, got {}:{}",
        fault.file,
        fault.line
    );
    assert!(fault.line > 0);
    let slot = node.dump_flight().pop().expect("ring slot");
    assert!(slot.file.ends_with("regression_postmortems.rs"));
}

#[test]
fn trap2_guard_drop_carries_arming_coordinates() {
    // Guard path: the implicit-drop record reuses the ARMING coordinates
    // (enter site), so the ring points at the forgotten commit.
    let node = SnubberNode::builder("trap2_guard").build();
    let g = node.enter(9, 9, || {}).expect("admit");
    drop(g);
    let fault = node.stats().last_fault.expect("recorded");
    assert!(
        fault.file.ends_with("regression_postmortems.rs"),
        "arming coords, got {}:{}",
        fault.file,
        fault.line
    );
}

#[test]
fn trap3_depth_zero_clamps_to_one_and_records() {
    // `flight_depth(0)` clamps to 1: the ring stays live (never a
    // with_capacity(0) edge), keeps exactly the newest fault.
    let node = SnubberNode::builder("trap3_zero").flight_depth(0).build();
    assert_eq!(node.flight_depth(), 1);
    node.report(FaultKind::Panic, 1, 1);
    node.report(FaultKind::Timeout, 2, 2);
    let dump = node.dump_flight();
    assert_eq!(dump.len(), 1, "clamped ring keeps newest only");
    assert_eq!(dump[0].kind, FaultKind::Timeout);
    assert_eq!(node.stats().poison_recoveries, 0);
}

#[test]
fn trap3_depth_huge_clamps_to_cap_and_evicts() {
    // `flight_depth(9999)` clamps to 64: bounded memory, oldest evicted.
    let node = SnubberNode::builder("trap3_huge")
        .flight_depth(9999)
        .build();
    assert_eq!(node.flight_depth(), 64);
    for i in 1..=70u64 {
        node.report(FaultKind::StaleEpoch, i, i);
    }
    let dump = node.dump_flight();
    assert_eq!(dump.len(), 64);
    assert_eq!(dump[0].epoch, 7, "oldest-first after eviction");
    assert_eq!(dump[63].epoch, 70);
}

#[test]
fn trap4_ring_never_grows_past_cap_under_flood() {
    // 10 000 faults at depth 16: len pinned, capacity stable, newest kept.
    let node = SnubberNode::builder("trap4_flood").flight_depth(16).build();
    for i in 1..=10_000u64 {
        node.report(FaultKind::BufferOverflow, i, i);
    }
    let dump = node.dump_flight();
    assert_eq!(dump.len(), 16);
    assert_eq!(dump[0].epoch, 9985);
    assert_eq!(dump[15].epoch, 10_000);
    assert_eq!(node.stats().poison_recoveries, 0);
}

#[test]
fn trap4_mixed_paths_share_one_bounded_ring() {
    // Guard drops + reports interleave into ONE ring that never exceeds
    // depth and stays oldest-first. `trip_after_failures(0)` keeps the
    // breaker OFF so all 12 faults land (guard drops would trip it).
    let node = SnubberNode::builder("trap4_mixed")
        .flight_depth(8)
        .trip_after_failures(0)
        .build();
    for i in 1..=5u64 {
        node.report(FaultKind::Panic, i, i);
    }
    for i in 6..=8u64 {
        let g = node.enter(i, i, || {}).expect("admit");
        drop(g);
    }
    for i in 9..=12u64 {
        node.report(FaultKind::BulkheadFull, i, i);
    }
    let dump = node.dump_flight();
    assert_eq!(dump.len(), 8);
    assert_eq!(dump[0].epoch, 5, "epochs 1..4 evicted, 5 oldest kept");
    assert_eq!(dump[7].epoch, 12);
    assert_eq!(dump[7].kind, FaultKind::BulkheadFull);
    assert_eq!(node.stats().poison_recoveries, 0);
}
