//! Execution envelope: a corridor with two walls.
//!
//! A budget catches "too slow / too big". The envelope also catches "too
//! fast / too small" — the silent skip, the stale cache hit, the loop that
//! never ran. Floors and ceilings are counted in their own stats columns and
//! never feed the breaker.
//!
//! Clocks are fake (`AtomicU64` ns) so every duration is exact.

use snubber::{
    BreakerState, EnvelopeVerdict, ExecutionEnvelope, PayloadVerdict, SnubberNode, TimeVerdict,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static CLOCK: AtomicU64 = AtomicU64::new(0);
static CLOCK_LOCK: Mutex<()> = Mutex::new(());

fn clock() -> u64 {
    CLOCK.load(Ordering::SeqCst)
}

#[test]
fn inside_corridor_counts_nothing() {
    let _gate = CLOCK_LOCK.lock().expect("clock lock");
    CLOCK.store(0, Ordering::SeqCst);

    let env = ExecutionEnvelope::new(100, 1_000, 10, 100);
    let node = SnubberNode::builder("env-inside")
        .execution_envelope(env)
        .build();

    let guard = node
        .enter_profiled_with_clock(clock, None, || {})
        .expect("admit");
    CLOCK.store(500_000, Ordering::SeqCst);
    let (event, profile, verdict) = guard.commit_enveloped(50);

    assert!(event.is_terminal());
    assert_eq!(profile.work_wall_us, 500);
    assert_eq!(verdict, EnvelopeVerdict::inside());
    let stats = node.stats();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.envelope_underflows, 0);
    assert_eq!(stats.envelope_overflows, 0);
}

#[test]
fn underflow_is_a_signal_not_a_failure() {
    let _gate = CLOCK_LOCK.lock().expect("clock lock");
    CLOCK.store(0, Ordering::SeqCst);

    let env = ExecutionEnvelope::new(100, 0, 10, 0);
    let node = SnubberNode::builder("env-under")
        .execution_envelope(env)
        .build();

    let guard = node
        .enter_profiled_with_clock(clock, None, || {})
        .expect("admit");
    CLOCK.store(50_000, Ordering::SeqCst);
    let (_event, profile, verdict) = guard.commit_enveloped(5);

    assert_eq!(profile.work_wall_us, 50);
    assert_eq!(
        verdict.time,
        TimeVerdict::Under {
            wall_us: 50,
            min_us: 100
        }
    );
    assert_eq!(verdict.payload, PayloadVerdict::Under { bytes: 5, min: 10 });
    assert!(verdict.has_underflow());
    assert!(!verdict.has_overflow());
    assert!(!verdict.is_inside());

    let stats = node.stats();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.envelope_underflows, 1);
    assert_eq!(stats.envelope_overflows, 0);
    assert_eq!(stats.implicit_drops, 0);
    assert_eq!(stats.timeouts_fired, 0);
    assert_eq!(
        stats.breaker_state,
        BreakerState::Closed,
        "a floor miss must never feed the breaker"
    );
}

#[test]
fn overflow_is_counted_in_its_own_column() {
    let _gate = CLOCK_LOCK.lock().expect("clock lock");
    CLOCK.store(0, Ordering::SeqCst);

    let env = ExecutionEnvelope::new(0, 100, 0, 10);
    let node = SnubberNode::builder("env-over")
        .execution_envelope(env)
        .build();

    let guard = node
        .enter_profiled_with_clock(clock, None, || {})
        .expect("admit");
    CLOCK.store(500_000, Ordering::SeqCst);
    let (_event, _profile, verdict) = guard.commit_enveloped(50);

    assert_eq!(
        verdict.time,
        TimeVerdict::Over {
            wall_us: 500,
            max_us: 100
        }
    );
    assert_eq!(verdict.payload, PayloadVerdict::Over { bytes: 50, max: 10 });
    assert!(verdict.has_overflow());
    assert!(!verdict.has_underflow());

    let stats = node.stats();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.envelope_overflows, 1);
    assert_eq!(stats.envelope_underflows, 0);
    assert_eq!(stats.breaker_state, BreakerState::Closed);
}

#[test]
fn zero_disables_one_side_only() {
    let floors_only = ExecutionEnvelope::new(0, 0, 10, 0);

    assert_eq!(
        floors_only.time_verdict(10_000_000),
        TimeVerdict::Inside,
        "max == 0 is no ceiling"
    );
    assert_eq!(
        floors_only.payload_verdict(0),
        PayloadVerdict::Under { bytes: 0, min: 10 }
    );
    assert_eq!(
        floors_only.payload_verdict(1_000_000),
        PayloadVerdict::Inside,
        "max_payload == 0 is no ceiling"
    );
    assert_eq!(
        floors_only.judge(50, 50),
        EnvelopeVerdict::inside(),
        "judge and the const constructor agree"
    );

    let ceiling_only = ExecutionEnvelope::new(0, 100, 0, 10);
    assert_eq!(
        ceiling_only.time_verdict(100),
        TimeVerdict::Inside,
        "bounds are inclusive"
    );
    assert_eq!(
        ceiling_only.time_verdict(101),
        TimeVerdict::Over {
            wall_us: 101,
            max_us: 100
        }
    );
    assert_eq!(ceiling_only.payload_verdict(10), PayloadVerdict::Inside);
    assert_eq!(
        ceiling_only.payload_verdict(11),
        PayloadVerdict::Over { bytes: 11, max: 10 }
    );
}

#[test]
fn ceiling_below_floor_clamps_up() {
    let env = ExecutionEnvelope::new(500, 100, 64, 8);
    assert_eq!(env.max_wall_us, 500, "floor is the stronger promise");
    assert_eq!(env.max_payload, 64);
    assert!(!env.is_off());

    assert!(ExecutionEnvelope::off().is_off());
    assert_eq!(ExecutionEnvelope::default(), ExecutionEnvelope::off());
    assert_eq!(std::mem::size_of::<ExecutionEnvelope>(), 16);
}

#[test]
fn plain_guard_judges_payload_but_has_no_clock() {
    let env = ExecutionEnvelope::new(100, 0, 10, 0);
    let node = SnubberNode::builder("env-plain")
        .execution_envelope(env)
        .build();

    let guard = node.enter(1, 1, || {}).expect("admit");
    let (_event, profile, verdict) = guard.commit_enveloped(5);

    assert_eq!(profile.work_wall_us, 0, "no profile clock was armed");
    assert_eq!(verdict.time, TimeVerdict::Inside);
    assert_eq!(verdict.payload, PayloadVerdict::Under { bytes: 5, min: 10 });
    assert_eq!(node.stats().envelope_underflows, 1);
}

#[test]
fn disabled_envelope_is_zero_cost_in_judgement() {
    let node = SnubberNode::builder("env-off").build();
    let guard = node.enter(1, 1, || {}).expect("admit");
    let (_event, _profile, verdict) = guard.commit_enveloped(0);
    assert_eq!(verdict, EnvelopeVerdict::inside());
    let stats = node.stats();
    assert_eq!(stats.envelope_underflows, 0);
    assert_eq!(stats.envelope_overflows, 0);
}
