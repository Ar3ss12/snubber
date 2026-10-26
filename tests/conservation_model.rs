//! Model-based admission conservation under MIXED policies.
//!
//! One scripted run drives four refusal/settle mechanisms through the same
//! node — `EvictOldest` overbooking, the `max + 1` bulkhead ceiling, a
//! `Critical`-over-`Background` preemption request, and a watchdog timeout —
//! and compares the live counters against an explicit ledger.
//!
//! The ledger is the point. Branch tests prove each arm counts SOMETHING;
//! this test proves the columns compose:
//!
//! * `admission_total() == total_started + shed_total()` — every attempt in
//!   exactly one bucket, no matter which mechanism produced it.
//! * `evicted` and `preempted` are side-events of admissions that ALREADY
//!   sit in `total_started`: adding them to the shed side would double-book.
//! * `reported[]` (per-`FaultKind`, the alert/metric ledger) is NOT a
//!   conservation term: it counts timeouts, implicit drops and explicit
//!   telemetry-only reports that never appeared on the admission boundary.
//!
//! The breaker is disabled (`trip_after_failures(0)`) so column arithmetic
//! stays independent of streak policy — the test is about conservation, not
//! about when a breaker trips.

use core::time::Duration;
use snubber::{
    BreakerState, EnterError, FaultKind, OverflowPolicy, Priority, SnubberEvent, SnubberNode,
    SnubberStats, TimeoutCheck,
};

/// Expected book position after every step of the script.
#[derive(Debug, Default)]
struct Ledger {
    started: u64,
    committed: u64,
    aborts: u64,
    drops: u64,
    timeouts: u64,
    evicted: u64,
    preempted: u64,
    bulkhead: u64,
    /// Explicit `report(BufferOverflow)` — alert ledger only, no column.
    reports: u64,
}

impl Ledger {
    fn check(&self, st: &SnubberStats) {
        // 1. Every mechanism column matches the script exactly.
        assert_eq!(st.total_started, self.started, "started admissions");
        assert_eq!(st.committed, self.committed, "commit paths");
        assert_eq!(st.explicit_aborts, self.aborts, "planned aborts");
        assert_eq!(st.implicit_drops, self.drops, "implicit drops");
        assert_eq!(st.timeouts_fired, self.timeouts, "watchdog settles");
        assert_eq!(st.evicted, self.evicted, "eviction side-events");
        assert_eq!(st.preempted, self.preempted, "preemption side-events");
        assert_eq!(st.bulkhead_rejections, self.bulkhead, "bulkhead refusals");

        // 2. Untouched mechanism columns stay at zero: this run must not
        //    leak an attempt into a bucket it never exercised.
        assert_eq!(st.rate_rejected, 0, "no token bucket in this model");
        assert_eq!(st.stale_epochs_rejected, 0, "no stale epochs");
        assert_eq!(st.breaker_rejections, 0, "breaker disabled");
        assert_eq!(st.poisoned_rejected, 0, "no invariant breaches");
        assert_eq!(st.memory_rejections, 0, "no memory pressure");
        assert_eq!(st.breaker_trips, 0, "trip_after_failures(0)");
        assert_eq!(st.breaker_state, BreakerState::Closed);
        assert_eq!(st.passthrough_admissions, 0, "canary lane unused");
        assert_eq!(st.window_outcomes, 0, "no failure window armed");

        // 3. The law itself, in the crate's own arithmetic.
        assert_eq!(
            st.admission_total(),
            st.total_started + st.shed_total(),
            "every attempt lands in exactly one bucket"
        );
        assert_eq!(st.shed_total(), self.bulkhead, "only bulkhead sheds here");
        assert_eq!(st.shed_bulkhead(), self.bulkhead);
        assert_eq!(st.shed_rate(), 0);
        assert_eq!(st.shed_stale(), 0);
        assert_eq!(st.shed_preempted(), self.preempted, "own column forever");
        assert_eq!(
            self.started + self.bulkhead,
            st.admission_total(),
            "caller-side attempts == started + refusals"
        );

        // 4. Side-events are NOT conservation terms: folding `evicted` or
        //    `preempted` into the shed side would double-book the admission
        //    that produced them and break the law above.
        assert_eq!(self.evicted, 1, "one overbook");
        assert_eq!(self.preempted, 1, "one cooperative request");
        assert_ne!(
            st.admission_total(),
            st.total_started + st.shed_total() + st.evicted + st.preempted,
            "evicted/preempted are side-events, never shedding buckets"
        );

        // 5. `reported[]` is the alert ledger, not a partition of attempts:
        //    it books timeouts, implicit drops and a telemetry-only
        //    BufferOverflow report that no admission boundary saw.
        let mut expected = [0u64; FaultKind::COUNT];
        expected[FaultKind::BulkheadFull.index()] = self.bulkhead;
        expected[FaultKind::EvictedOldest.index()] = self.evicted;
        expected[FaultKind::Preempted.index()] = self.preempted;
        expected[FaultKind::Timeout.index()] = self.timeouts;
        expected[FaultKind::ImplicitDrop.index()] = self.drops;
        expected[FaultKind::BufferOverflow.index()] = self.reports;
        assert_eq!(st.reported, expected, "every fault booked exactly once");
        let recorded: u64 = st.reported.iter().sum();
        assert_ne!(
            recorded,
            st.admission_total(),
            "reported[] counts side-events and telemetry-only records"
        );

        // 6. Derived views agree with the ledger.
        assert_eq!(
            st.catastrophic_faults(),
            self.timeouts + self.drops,
            "only the timeout and the implicit drop are catastrophic"
        );
        assert_eq!(
            st.success_rate_per_mille(),
            self.committed * 1000 / self.started,
            "success ratio uses started, not attempts"
        );
        assert_eq!(
            st.depth_at(2),
            u8::try_from((self.started - self.committed - self.aborts - self.drops) * 255 / 2)
                .expect("depth fits u8"),
            "in-flight gauge derives from the same books"
        );
        assert_eq!(st.queue_depth, 0, "no parked waiters in this run");
        assert_eq!(st.poison_recoveries, 0, "zero-panic law");
    }
}

#[test]
fn mixed_policy_columns_conserve_every_attempt() {
    let node = SnubberNode::builder("model/conservation")
        .max_in_flight(2)
        .overflow(OverflowPolicy::EvictOldest)
        .trip_after_failures(0)
        .timeout_ms(1)
        .build();
    let mut model = Ledger::default();

    // 1-2. Fill both slots with pure `Background` occupancy (precondition
    //      for the cooperative Critical-over-Background preemption below).
    let bg_a = node
        .enter_with_priority(Priority::Background, 1, 1, || {})
        .expect("slot 1 admits");
    let bg_b = node
        .enter_with_priority(Priority::Background, 2, 2, || {})
        .expect("slot 2 admits");
    model.started += 2;
    assert_eq!(node.lane_in_flight(), [2, 0, 0]);

    // 3. `EvictOldest`: the third Background admission overbooks by exactly
    //    one (`max + 1`) and books the eviction as a side-event.
    let bg_c = node
        .enter_with_priority(Priority::Background, 3, 3, || {})
        .expect("EvictOldest admits the newcomer over capacity");
    model.started += 1;
    model.evicted += 1;
    assert_eq!(node.lane_in_flight(), [3, 0, 0], "one legal overbook");

    // 4. Ceiling reached: a fourth Background is refused, never overbooked
    //    twice.
    match node.enter_with_priority(Priority::Background, 4, 4, || {}) {
        Err(EnterError::BulkheadFull) => model.bulkhead += 1,
        Ok(_) => panic!("max + 1 must refuse the fourth admission"),
        Err(other) => panic!("unexpected refusal: {other:?}"),
    }

    // 5. Critical over pure Background: ONE cooperative preemption request
    //    plus the same BulkheadFull refusal. The victim yields on its own
    //    clock — the counter is a request, not an admission.
    match node.enter_with_priority(Priority::Critical, 5, 5, || {}) {
        Err(EnterError::BulkheadFull) => model.bulkhead += 1,
        Ok(_) => panic!("node never forces an uncooperative victim"),
        Err(other) => panic!("unexpected refusal: {other:?}"),
    }
    model.preempted += 1;
    assert!(node.preempt_requested(), "the knock filed a request");

    // 6. Watchdog: the oldest guard blows its wall lease and settles as a
    //    timeout — rollback runs, the slot returns, the streak column moves.
    std::thread::sleep(Duration::from_millis(20));
    match bg_a.check_timeout() {
        Err(TimeoutCheck::TimedOut) => model.timeouts += 1,
        Err(other) => panic!("wall-only silence must be TimedOut, got {other:?}"),
        Ok(_) => panic!("20 ms of silence must blow a 1 ms lease"),
    }
    assert_eq!(node.lane_in_flight(), [2, 0, 0]);
    assert!(
        !node.preempt_requested(),
        "a Background settle clears the request"
    );

    // 7. Commit frees a slot; the next admission passes with no side-event.
    match bg_b.commit() {
        SnubberEvent::Committed { .. } => model.committed += 1,
        other => panic!("commit must settle as Committed, got {other:?}"),
    }
    let late = node
        .enter_with_priority(Priority::Background, 6, 6, || {})
        .expect("slot freed by the commit");
    model.started += 1;
    assert_eq!(node.lane_in_flight(), [2, 0, 0]);

    // 8. Planned abort: rollback NOW, explicit column, no failure streak.
    match bg_c.abort("planned rollback") {
        SnubberEvent::Aborted { reason, .. } => {
            assert_eq!(reason, "planned rollback");
            model.aborts += 1;
        }
        other => panic!("abort must settle as Aborted, got {other:?}"),
    }

    // 9. Implicit drop is a pure bug path: its own column AND catastrophic.
    drop(late);
    model.drops += 1;
    assert_eq!(node.lane_in_flight(), [0, 0, 0], "every slot came home");

    // 10. Telemetry-only report: the alert ledger grows, the conservation
    //     arithmetic does not move.
    let before = node.stats();
    node.report(FaultKind::BufferOverflow, 7, 7);
    model.reports += 1;
    let st = node.stats();
    assert_eq!(
        st.admission_total(),
        before.admission_total(),
        "a report is not an admission"
    );
    assert_eq!(st.shed_total(), before.shed_total());

    model.check(&st);
    assert_eq!(
        st.last_fault.as_ref().map(|f| f.kind),
        Some(FaultKind::BufferOverflow)
    );

    println!(
        "\n[MODEL]: started={} committed={} aborts={} drops={} timeouts={} evicted={} preempted={} bulk={} | admission_total={} == started+shed={} | reported_sum={}",
        st.total_started,
        st.committed,
        st.explicit_aborts,
        st.implicit_drops,
        st.timeouts_fired,
        st.evicted,
        st.preempted,
        st.bulkhead_rejections,
        st.admission_total(),
        st.total_started + st.shed_total(),
        st.reported.iter().sum::<u64>(),
    );
}
