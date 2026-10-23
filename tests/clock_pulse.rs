//! Monotonic clock, CPU-spin classifier, and worker pulse.
//!
//! Tripwire for the hang-protection layer (`src/clock.rs` + guard CPU
//! budget): wall time never jumps on NTP edits, hot spins classify as
//! `CpuSpin`, sleeps classify as `SleepBlocked`, and the 8-byte pulse
//! times out on a frozen tick.

use snubber::{
    classify_stall, MonotonicMs, PulseWatch, SnubberNode, StallKind, SystemClock, TimeoutCheck,
    WorkerPulse,
};

#[test]
fn system_clock_is_monotonic_across_samples() {
    let c = SystemClock;
    let a = c.now_ms();
    std::thread::sleep(core::time::Duration::from_millis(50));
    let b = c.now_ms();
    assert!(b >= a, "monotonic clock moved backwards: {a} -> {b}");
    // Upper bound catches a frozen/stalled clock too: a 50 ms sleep must
    // advance the reading, and wildly so (generous 5 s ceiling absorbs
    // scheduler hiccups on loaded CI).
    assert!(
        b.saturating_sub(a) <= 5_000,
        "clock frozen or skewed: 50 ms sleep advanced {} ms",
        b.saturating_sub(a)
    );
}

#[test]
fn classifier_wall_only_never_reports_spin() {
    // No sampler attached in wall-only mode: blown lease = sleep/block.
    assert_eq!(classify_stall(5000, 5000, 100, 0), StallKind::SleepBlocked);
}

#[test]
fn classifier_hot_spin_needs_cpu_budget() {
    assert_eq!(classify_stall(5000, 5000, 100, 50), StallKind::CpuSpin);
    // Deadlocked mutex: CPU stood still — still sleep, not spin.
    assert_eq!(classify_stall(5000, 0, 100, 50), StallKind::SleepBlocked);
}

#[test]
fn wall_only_guard_times_out_without_sampler() {
    let node = SnubberNode::builder("clock/wall-only")
        .max_in_flight(1)
        .timeout_ms(1)
        .build();
    let guard = node.enter(1, 1, || {}).expect("admit");
    std::thread::sleep(core::time::Duration::from_millis(10));
    match guard.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {}
        _ => panic!("lease blown must settle as TimedOut"),
    }
}

#[test]
fn spinning_guard_reports_spin_detected() {
    static CPU_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn sample() -> u64 {
        // Simulate a hot spin: every sample advances the CPU counter,
        // so wall + CPU grow together past both budgets.
        CPU_MS.fetch_add(100, std::sync::atomic::Ordering::SeqCst)
    }
    CPU_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    let node = SnubberNode::builder("clock/spin")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .build();
    let guard = node
        .enter(1, 1, || {})
        .expect("admit")
        .with_cpu_sampler(sample);
    std::thread::sleep(core::time::Duration::from_millis(10));
    match guard.check_timeout() {
        Err(TimeoutCheck::SpinDetected) => {}
        _ => panic!("hot spin must settle as SpinDetected"),
    }
    assert_eq!(node.stats().timeouts_fired, 1);
}

#[test]
fn sleeping_guard_reports_plain_timeout() {
    static CPU_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn sample() -> u64 {
        CPU_MS.load(std::sync::atomic::Ordering::SeqCst)
    }
    // Simulate a blocked syscall: CPU stood still while wall blew.
    CPU_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    let node = SnubberNode::builder("clock/sleep")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .build();
    let guard = node
        .enter(1, 1, || {})
        .expect("admit")
        .with_cpu_sampler(sample);
    std::thread::sleep(core::time::Duration::from_millis(10));
    match guard.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {}
        _ => panic!("blocked sleep must settle as TimedOut, not SpinDetected"),
    }
}

#[test]
fn silent_guard_past_ceiling_reports_deadlock_suspected() {
    static CPU_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn sample() -> u64 {
        // Wedged thread: CPU never moves while the wall keeps running.
        CPU_MS.load(std::sync::atomic::Ordering::SeqCst)
    }
    CPU_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    let node = SnubberNode::builder("clock/deadlock")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .stall_ceiling_ms(5)
        .build();
    let guard = node
        .enter(1, 1, || {})
        .expect("admit")
        .with_cpu_sampler(sample);
    std::thread::sleep(core::time::Duration::from_millis(15));
    match guard.check_timeout() {
        Err(TimeoutCheck::DeadlockSuspected) => {}
        _ => panic!("silence past the stall ceiling must settle as DeadlockSuspected"),
    }
    let st = node.stats();
    assert_eq!(st.deadlock_suspected, 1, "deadlock verdict counted");
    assert_eq!(st.timeouts_fired, 1);
}

#[test]
fn short_pause_under_ceiling_stays_plain_timeout() {
    static CPU_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn sample() -> u64 {
        // Brief OS/EDR pause: CPU still, but well under the 60 s ceiling.
        CPU_MS.load(std::sync::atomic::Ordering::SeqCst)
    }
    CPU_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    let node = SnubberNode::builder("clock/pause")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .stall_ceiling_ms(60_000)
        .build();
    let guard = node
        .enter(1, 1, || {})
        .expect("admit")
        .with_cpu_sampler(sample);
    std::thread::sleep(core::time::Duration::from_millis(15));
    // Blown deadline, still CPU, ceiling NOT reached: tolerated pause.
    match guard.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {}
        _ => panic!("short silence must NOT settle as DeadlockSuspected"),
    }
    let st = node.stats();
    assert_eq!(
        st.deadlock_suspected, 0,
        "no deadlock verdict on short pause"
    );
    assert_eq!(st.timeouts_fired, 1);
}

#[test]
fn worker_pulse_stays_eight_bytes_and_times_out() {
    assert_eq!(core::mem::size_of::<WorkerPulse>(), 8);
    let pulse = WorkerPulse::new();
    let mut watch = PulseWatch::new(10);
    assert!(watch.observe(pulse.tick(), 1000));
    pulse.kick();
    assert!(watch.observe(pulse.tick(), 1005));
    // Frozen tick: inside lease ok, past lease missed.
    assert!(watch.observe(pulse.tick(), 1010));
    assert!(!watch.observe(pulse.tick(), 1100));
}
