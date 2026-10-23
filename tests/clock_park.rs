//! std-side contract tests for the firmware-facing trait family in
//! `clock.rs`: `CpuClock`, `IdlePark`, `WdtSink` (+ the std impls
//! `NoWdt`, `SpinPark`).
//!
//! On `std` paths these traits are compile-checked only — the OS
//! supervisor owns liveness and the worker parks on a `Condvar`. This
//! file pins the documented semantics with fake impls (exactly what a
//! firmware author writes on the other side), so a signature or
//! contract change fails the suite instead of silently breaking
//! out-of-tree firmware impls.

use snubber::{CpuClock, IdlePark, NoWdt, SpinPark, WdtSink};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static FEEDS: AtomicU64 = AtomicU64::new(0);
static PARKS: AtomicU64 = AtomicU64::new(0);
static WALL_TICKS: AtomicU64 = AtomicU64::new(0);
static CPU_TICKS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct CountingWdt;
impl WdtSink for CountingWdt {
    fn feed(self) {
        FEEDS.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
struct RecordingPark;
impl IdlePark for RecordingPark {
    fn park(self) {
        PARKS.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
struct FakeCpuClock;
impl CpuClock for FakeCpuClock {
    fn thread_cpu_ms(self) -> u64 {
        CPU_TICKS.load(Ordering::Relaxed)
    }
    fn wall_ms(self) -> u64 {
        WALL_TICKS.load(Ordering::Relaxed)
    }
}

#[test]
fn no_wdt_feed_is_a_noop() {
    let w = NoWdt;
    for _ in 0..1_000 {
        w.feed();
    }
}

#[test]
fn spin_park_is_a_hint_not_a_sleep() {
    let start = Instant::now();
    for _ in 0..1_000 {
        SpinPark.park();
    }
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "SpinPark::park is one spin_loop hint; a sleep snuck in"
    );
}

#[test]
fn custom_wdt_sink_receives_every_feed() {
    FEEDS.store(0, Ordering::Relaxed);
    let w = CountingWdt;
    w.feed();
    w.feed();
    w.feed();
    assert_eq!(
        FEEDS.load(Ordering::Relaxed),
        3,
        "WdtSink::feed must fire once per kick (idempotent stroking is the impl's duty)"
    );
}

#[test]
fn custom_idle_park_is_the_idle_point() {
    PARKS.store(0, Ordering::Relaxed);
    let p = RecordingPark;
    p.park();
    p.park();
    assert_eq!(
        PARKS.load(Ordering::Relaxed),
        2,
        "IdlePark::park is the single idle point; the caller re-checks its directives after return"
    );
}

#[test]
fn cpu_clock_reads_are_a_paired_sample() {
    WALL_TICKS.store(100, Ordering::Relaxed);
    CPU_TICKS.store(40, Ordering::Relaxed);
    let c = FakeCpuClock;
    assert_eq!(c.wall_ms(), 100);
    assert_eq!(
        c.thread_cpu_ms(),
        40,
        "CpuClock exposes wall + cpu as one paired sample (same backend)"
    );
    // Monotonic forward move: both legs only grow.
    WALL_TICKS.store(150, Ordering::Relaxed);
    CPU_TICKS.store(60, Ordering::Relaxed);
    assert!(c.wall_ms() >= 100);
    assert!(c.thread_cpu_ms() >= 40);
}
