//! Layer 2 (alloc): the HOLDING erased admission keeps its bulkhead slot
//! until settle, so `max_in_flight` becomes a ceiling over live erased
//! transactions. The PROBE admission still releases at entry — both shapes
//! are pinned here, side by side, because the distinction is the whole
//! point of the 0.7.0 split.
//!
//! Positive and negative: the holding path must refuse a second admission
//! past the ceiling, release exactly one slot on `commit`, on `Drop` and on
//! a panicking rollback, and leave the conservation equation balanced.

#![cfg(feature = "alloc")]

use snubber::{EnterError, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};

/// The holding admission occupies its lane: `max_in_flight(1)` refuses a
/// second live erased transaction with `BulkheadFull`, and committing the
/// first one frees exactly that one ceiling seat.
#[test]
fn holding_bounds_live_transactions() {
    let node = SnubberNode::builder("erased/holding/ceiling")
        .max_in_flight(1)
        .build();

    let first = node
        .enter_erased_holding_auto(|| {})
        .expect("first holding admission");
    assert_eq!(
        node.lane_in_flight(),
        [0, 1, 0],
        "the held slot is booked on the Normal lane"
    );

    let second = node.enter_erased_holding_auto(|| {});
    assert!(
        matches!(second, Err(EnterError::BulkheadFull)),
        "a second live erased transaction past the ceiling must be refused, got {second:?}"
    );
    assert_eq!(
        node.stats().bulkhead_rejections,
        1,
        "the refusal is booked as a bulkhead rejection, not a panic"
    );

    first.commit();
    assert_eq!(node.lane_in_flight(), [0, 0, 0], "commit returned the slot");

    node.enter_erased_holding_auto(|| {})
        .expect("admitted after the release")
        .commit();
    assert_eq!(node.stats().committed, 2);
    assert_eq!(node.stats().total_started, 2, "refusals book no admission");
}

/// `commit()` releases the slot exactly once: the next admission is granted
/// and `in_flight` never goes negative or double-releases (a second release
/// would surface as a lane underflow in the conservation check below).
#[test]
fn holding_releases_once_on_commit() {
    let node = SnubberNode::builder("erased/holding/commit")
        .max_in_flight(2)
        .build();

    let a = node.enter_erased_holding_auto(|| {}).expect("admit a");
    let b = node.enter_erased_holding_auto(|| {}).expect("admit b");
    assert_eq!(node.lane_in_flight(), [0, 2, 0]);

    a.commit();
    assert_eq!(
        node.lane_in_flight(),
        [0, 1, 0],
        "exactly one slot released"
    );
    b.commit();
    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "exactly one more released"
    );

    let stats = node.stats();
    assert_eq!(stats.total_started, 2);
    assert_eq!(stats.committed, 2);
    assert_eq!(stats.explicit_aborts, 0);
    assert_eq!(stats.implicit_drops, 0);
}

/// Dropping a holding guard is a planned abort: the user rollback fires
/// once, the slot comes home and the depth gauge does not keep a ghost.
#[test]
fn holding_releases_once_on_drop() {
    static RELEASES: AtomicU64 = AtomicU64::new(0);
    let node = SnubberNode::builder("erased/holding/drop")
        .max_in_flight(1)
        .build();

    {
        let _g = node
            .enter_erased_holding_auto(|| {
                RELEASES.fetch_add(1, Ordering::SeqCst);
            })
            .expect("admit");
        assert_eq!(node.lane_in_flight(), [0, 1, 0], "held while alive");
    }

    assert_eq!(RELEASES.load(Ordering::SeqCst), 1, "rollback exactly once");
    assert_eq!(node.lane_in_flight(), [0, 0, 0], "Drop returned the slot");
    let stats = node.stats();
    assert_eq!(stats.explicit_aborts, 1, "Drop is a planned abort");
    assert_eq!(stats.implicit_drops, 0, "no phantom implicit drop");
    assert_eq!(stats.committed, 0);
    assert_eq!(stats.depth_at(4), 0, "no ghost pending work");

    node.enter_erased_holding_auto(|| {})
        .expect("a dropped holding guard frees the ceiling")
        .commit();
}

/// A panicking user rollback must not take the slot with it: the guard's
/// `catch_unwind` swallows the panic and the settle ran BEFORE the unwind,
/// so the ceiling is usable afterwards.
#[test]
fn holding_releases_slot_on_panicking_rollback() {
    let node = SnubberNode::builder("erased/holding/panic")
        .max_in_flight(1)
        .build();

    let guard = node
        .enter_erased_holding_auto(|| panic!("rollback explodes"))
        .expect("admit");
    drop(guard); // panic swallowed inside ErasedGuard::drop

    assert_eq!(
        node.lane_in_flight(),
        [0, 0, 0],
        "the slot must settle before the panic resumes"
    );
    assert_eq!(node.stats().explicit_aborts, 1);

    node.enter_erased_holding_auto(|| {})
        .expect("ceiling usable after the panicking rollback")
        .commit();
}

/// The PROBE path is unchanged by the split: it releases at entry, so a
/// live probe guard does not consume a ceiling seat.
#[test]
fn probe_still_releases_at_entry() {
    let node = SnubberNode::builder("erased/probe/entry")
        .max_in_flight(1)
        .build();

    let a = node.probe_erased_auto(|| {}).expect("probe a");
    assert_eq!(node.lane_in_flight(), [0, 0, 0], "probe released at entry");
    let b = node
        .probe_erased_auto(|| {})
        .expect("a live probe guard must not occupy the ceiling");
    assert_eq!(node.lane_in_flight(), [0, 0, 0], "both probes released");

    a.commit();
    b.commit();
    assert_eq!(
        node.stats().committed,
        2,
        "each commit books its own outcome"
    );
}

/// Conservation: every admission ends in exactly one column, and the lane
/// histogram returns to zero — probes, holding commits and holding aborts
/// in one trace.
#[test]
fn holding_and_probe_conserve_every_attempt() {
    let node = SnubberNode::builder("erased/conservation")
        .max_in_flight(2)
        .build();

    let probe = node.probe_erased_auto(|| {}).expect("probe");
    let held = node.enter_erased_holding_auto(|| {}).expect("held");
    assert_eq!(node.lane_in_flight(), [0, 1, 0], "only the holding seat");

    // Fill the ceiling: the third holding admission is refused.
    let held2 = node
        .enter_erased_holding_auto(|| {})
        .expect("second holding seat");
    assert_eq!(node.lane_in_flight(), [0, 2, 0]);
    assert!(
        matches!(
            node.enter_erased_holding_auto(|| {}),
            Err(EnterError::BulkheadFull)
        ),
        "ceiling full"
    );

    probe.commit();
    held.commit();
    drop(held2);

    let stats = node.stats();
    assert_eq!(stats.total_started, 3, "three admissions booked");
    assert_eq!(stats.committed, 2, "probe + held commit");
    assert_eq!(stats.explicit_aborts, 1, "drop of held2");
    assert_eq!(stats.implicit_drops, 0);
    assert_eq!(
        stats.total_started,
        stats.committed + stats.explicit_aborts + stats.implicit_drops,
        "every attempt settled in exactly one column"
    );
    assert_eq!(node.lane_in_flight(), [0, 0, 0], "all slots came home");
}
