//! Monotonic time sources, CPU-spin classifier, and worker pulse.
//!
//! Core-only (`core` + atomics, no `std`, no `alloc`, no `unsafe`):
//! pure traits and integer math so bare-metal firmware (`SysTick`,
//! `DWT_CYCCNT`) and OS daemons (`Instant`, `GetThreadTimes`,
//! `clock_gettime`) share one decision language.
//!
//! Split of responsibilities:
//! * This module: `MonotonicMs` (tick source), `CpuClock`
//!   (thread CPU time source), [`classify_stall`] (SPIN vs SLEEP math),
//!   [`WorkerPulse`] / [`PulseWatch`] (8-byte heartbeat), [`WdtSink`]
//!   (hardware watchdog feed hook).
//! * Application: provides the `unsafe` backends. The crate never
//!   touches `unsafe` or OS handles — see `contract.rs` boundary.

#[cfg(not(target_has_atomic = "64"))]
use core::sync::atomic::AtomicU32;
#[cfg(target_has_atomic = "64")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;

#[cfg(target_has_atomic = "64")]
type PulseTick = AtomicU64;
/// Targets without 64-bit CAS (Cortex-M0..M7) keep a wrapping 32-bit
/// generation: the watcher only compares for equality, so a wrap is
/// invisible unless two observations are exactly 2^32 kicks apart.
#[cfg(not(target_has_atomic = "64"))]
type PulseTick = AtomicU32;

/// Millisecond tick source. Monotonic (never moves backwards).
///
/// Contract: `now_ms()` MUST NOT jump backwards on NTP sync, DST change,
/// or manual clock edits — unlike `SystemTime`. `std` provides
/// [`SystemClock`](crate::SystemClock); firmware provides a
/// `SysTick`-backed counter.
pub trait MonotonicMs: Copy {
    /// Current tick in milliseconds (arbitrary origin, monotonic).
    fn now_ms(self) -> u64;
}

/// Thread CPU-time source: ms the thread really spent on-CPU
/// (user + kernel), excluding sleeps and blocks.
///
/// Backends live in the application (`GetThreadTimes` on Windows,
/// `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` on Linux, `DWT_CYCCNT`
/// delta on Cortex-M). Core holds only the trait.
pub trait CpuClock: Copy {
    /// Ms the observed thread spent on-CPU since an arbitrary origin.
    fn thread_cpu_ms(self) -> u64;
    /// Wall ms from the same backend (paired sample).
    fn wall_ms(self) -> u64;
}

/// Stall verdict from one `(wall_delta, cpu_delta)` sample pair.
/// Pure `const` math — no clock reads, no syscalls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StallKind {
    /// Within budget — keep working.
    WithinBudget,
    /// Wall budget blown but CPU stood still and silence is below the
    /// stall ceiling: sleep in a syscall / `Condvar` / driver wait /
    /// EDR scan. Do NOT treat as a hot spin.
    SleepBlocked,
    /// CPU time grew with the wall: `loop {}` spin. De-energize iron
    /// past the thread, never kill the thread itself.
    CpuSpin,
    /// Wall silence at/above the stall ceiling with no CPU growth:
    /// wedged lock, dead pipe, or over-long OS suspension. Same
    /// cooperative settle as the other blown budgets, distinct verdict
    /// for the post-mortem dump.
    DeadlockStall,
}
/// Classify one stall sample.
///
/// * `wall_delta_ms` — wall time elapsed since arming.
/// * `cpu_delta_ms` — CPU time consumed in the same window.
/// * `wall_budget_ms` — wall lease (`0` = disabled).
/// * `cpu_budget_ms` — CPU lease (`0` = wall-only mode: a blown wall
///   lease reports `SleepBlocked`, never `CpuSpin`).
/// * `stall_ceiling_ms` — silence ceiling (`0` = disabled, identical to
///   the two-budget behavior): a blown wall lease with still CPU at or
///   above the ceiling reports `DeadlockStall` instead of `SleepBlocked`.
///
/// Order is load-bearing: `CpuSpin` is checked before `DeadlockStall`,
/// so a hot loop with huge wall time still reports spin, not silence.
/// A deadlocked `Mutex` wait shows `cpu_delta == 0` (thread sleeps):
/// below the ceiling it reports `SleepBlocked` (tolerated pause), at or
/// above the ceiling it reports `DeadlockStall` (wedged, settle it).
#[inline]
#[must_use]
pub const fn classify_stall(
    wall_delta_ms: u64,
    cpu_delta_ms: u64,
    wall_budget_ms: u64,
    cpu_budget_ms: u64,
) -> StallKind {
    classify_stall_ceiling(
        wall_delta_ms,
        cpu_delta_ms,
        wall_budget_ms,
        cpu_budget_ms,
        0,
    )
}
/// Classify one stall sample with a silence ceiling.
/// See [`classify_stall`] for the parameter contract; `stall_ceiling_ms`
/// adds the third circuit (`0` = disabled).
#[inline]
#[must_use]
pub const fn classify_stall_ceiling(
    wall_delta_ms: u64,
    cpu_delta_ms: u64,
    wall_budget_ms: u64,
    cpu_budget_ms: u64,
    stall_ceiling_ms: u64,
) -> StallKind {
    if wall_budget_ms == 0 || wall_delta_ms <= wall_budget_ms {
        return StallKind::WithinBudget;
    }
    if cpu_budget_ms != 0 && cpu_delta_ms >= cpu_budget_ms {
        return StallKind::CpuSpin;
    }
    if stall_ceiling_ms != 0 && wall_delta_ms >= stall_ceiling_ms {
        return StallKind::DeadlockStall;
    }
    StallKind::SleepBlocked
}

/// 8-byte worker heartbeat: the worker calls [`WorkerPulse::kick`] once
/// per loop iteration (one `Release` store); an independent watcher
/// observes via [`PulseWatch`].
///
/// The watcher MUST NOT kill the worker thread or drop its guard from
/// outside (poisoned mutexes, leaked allocator state). On a missed pulse
/// it de-energizes iron past the thread and reports `Timeout` — the
/// worker settles itself at its next `check_timeout` checkpoint.
#[derive(Debug)]
pub struct WorkerPulse {
    tick: PulseTick,
}

impl WorkerPulse {
    /// New pulse, tick `0` (never kicked).
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tick: PulseTick::new(0),
        }
    }

    /// Heartbeat: advance the generation counter (wrapping).
    #[inline]
    pub fn kick(&self) {
        self.tick.fetch_add(1, Ordering::Release);
    }

    /// Current tick (for the watcher side). 64-bit monotonic counter;
    /// targets without 64-bit atomics wrap at `2^32`.
    #[cfg(target_has_atomic = "64")]
    #[inline]
    pub fn tick(&self) -> u64 {
        self.tick.load(Ordering::Acquire)
    }

    /// Current tick (for the watcher side). Wraps at `2^32` on targets
    /// without 64-bit atomics.
    #[cfg(not(target_has_atomic = "64"))]
    #[inline]
    pub fn tick(&self) -> u64 {
        u64::from(self.tick.load(Ordering::Acquire))
    }
}

impl Default for WorkerPulse {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Watcher side of [`WorkerPulse`]: last seen tick + lease decision.
#[derive(Debug, Clone, Copy)]
pub struct PulseWatch {
    last_tick: u64,
    last_wall_ms: u64,
    lease_ms: u64,
}

impl PulseWatch {
    /// New watcher: first `observe` always passes (baseline capture).
    #[inline]
    #[must_use]
    pub const fn new(lease_ms: u64) -> Self {
        Self {
            last_tick: u64::MAX,
            last_wall_ms: 0,
            lease_ms,
        }
    }

    /// Observe one sample. `true` = tick advanced or lease intact;
    /// `false` = tick frozen past `lease_ms` (de-energize + report).
    #[inline]
    pub fn observe(&mut self, tick: u64, wall_now_ms: u64) -> bool {
        if self.last_tick == u64::MAX {
            self.last_tick = tick;
            self.last_wall_ms = wall_now_ms;
            return true;
        }
        if tick != self.last_tick {
            self.last_tick = tick;
            self.last_wall_ms = wall_now_ms;
            return true;
        }
        wall_now_ms.saturating_sub(self.last_wall_ms) <= self.lease_ms
    }

    /// Ms since the last tick advance (saturating).
    #[inline]
    #[must_use]
    pub const fn idle_ms(&self, wall_now_ms: u64) -> u64 {
        wall_now_ms.saturating_sub(self.last_wall_ms)
    }
}

/// Hardware watchdog feed hook.
///
/// The node/pulse calls `feed()` on every `kick`/`commit`. On `std` the
/// impl is a no-op (the OS supervisor owns liveness); on firmware it
/// strokes the real WDT (feed is idempotent, timing is caller's duty).
pub trait WdtSink: Copy {
    /// Stroke the watchdog (idempotent, non-blocking).
    fn feed(self);
}

/// No-op watchdog sink for OS targets.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWdt;

impl WdtSink for NoWdt {
    #[inline]
    fn feed(self) {}
}

/// Idle-park hook: sleep at 0% CPU until the next hardware event.
///
/// Split by target, one language:
/// * `std`: the worker parks on a `Condvar` (kernel sleep, already in
///   `TaskConduit::wait_while_paused` — this trait only names it).
/// * firmware: the task executes `wfi`/`wfe` (the core stops its clock
///   until an ISR lands). The `unsafe` `cortex_m::asm::wfi()` /
///   `cortex_m::asm::wfe()` call lives in YOUR app impl, never here.
///
/// `wfe` contract: correct only with `SCB.SCR.SEVONPEND = 1` (every
/// exception entry to pending sets the Event register) or with an
/// explicit `SEV` at the end of each ISR. With the reset value
/// `SEVONPEND = 0` an ISR that returns without `SEV` leaves Event = 0,
/// so `is_empty()` → `wfe` loses the wakeup and sleeps until the next
/// unrelated event. `SEVONPEND` is privileged: set it once at firmware
/// boot (app side). Any wakeup may be spurious — the caller always
/// re-checks its directive and bridge after `park()` returns.
///
/// Default `park` spins once (`core::hint::spin_loop`) so a bare
/// `core`-only target compiles with zero platform code; real firmware
/// overrides it with `wfi`/`wfe`.
pub trait IdlePark: Copy {
    /// Park until the next event (interrupt, notify, or spurious wake —
    /// the caller always re-checks its directive after return).
    fn park(self);
}

/// Spin-only park for targets without a sleep primitive (default).
#[derive(Debug, Clone, Copy, Default)]
pub struct SpinPark;

impl IdlePark for SpinPark {
    #[inline]
    fn park(self) {
        core::hint::spin_loop();
    }
}

#[cfg(target_has_atomic = "64")]
const _: () = {
    assert!(
        core::mem::size_of::<WorkerPulse>() == 8,
        "WorkerPulse must stay one AtomicU64 (8 bytes)"
    );
};
#[cfg(not(target_has_atomic = "64"))]
const _: () = {
    assert!(
        core::mem::size_of::<WorkerPulse>() == 4,
        "WorkerPulse must stay one AtomicU32 (4 bytes) without 64-bit CAS"
    );
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// tests exercise the panic paths on purpose; the unwinds ARE the contract
mod unit_tests {
    use super::*;

    #[test]
    fn stall_classifier_separates_spin_from_sleep() {
        assert_eq!(classify_stall(10, 0, 100, 50), StallKind::WithinBudget);
        assert_eq!(classify_stall(0, 0, 0, 50), StallKind::WithinBudget);
        assert_eq!(classify_stall(5000, 0, 100, 50), StallKind::SleepBlocked);
        assert_eq!(classify_stall(5000, 5000, 100, 0), StallKind::SleepBlocked);
        assert_eq!(classify_stall(5000, 5000, 100, 50), StallKind::CpuSpin);
        assert_eq!(classify_stall(200, 3, 100, 50), StallKind::SleepBlocked);
    }

    #[test]
    fn ceiling_splits_sleep_from_deadlock() {
        let c = classify_stall_ceiling;
        // Below the ceiling, still CPU: tolerated OS/EDR pause.
        assert_eq!(c(400, 0, 100, 50, 2000), StallKind::SleepBlocked);
        // At/above the ceiling, still CPU: wedged lock / dead pipe.
        assert_eq!(c(2000, 0, 100, 50, 2000), StallKind::DeadlockStall);
        assert_eq!(c(1_200_000, 5, 100, 50, 2000), StallKind::DeadlockStall);
        // Hot spin wins over silence at the same wall time.
        assert_eq!(c(5000, 5000, 100, 50, 2000), StallKind::CpuSpin);
        // ceiling = 0 disables the third circuit (legacy identity).
        assert_eq!(c(5000, 0, 100, 50, 0), StallKind::SleepBlocked);
        assert_eq!(c(5000, 0, 100, 0, 2000), StallKind::DeadlockStall);
        // Wall-only mode works with the ceiling (no sampler needed).
        assert_eq!(c(400, 0, 100, 0, 2000), StallKind::SleepBlocked);
        assert_eq!(c(3000, 0, 100, 0, 2000), StallKind::DeadlockStall);
        // budget=0 (no deadline) never classifies anything.
        assert_eq!(c(5000, 0, 0, 50, 2000), StallKind::WithinBudget);
    }

    #[test]
    fn pulse_watch_baselines_then_times_out() {
        let pulse = WorkerPulse::new();
        let mut watch = PulseWatch::new(100);
        assert!(watch.observe(pulse.tick(), 1000));
        pulse.kick();
        assert!(watch.observe(pulse.tick(), 1050));
        assert!(watch.observe(pulse.tick(), 1100));
        assert!(!watch.observe(pulse.tick(), 1200));
        assert_eq!(watch.idle_ms(1200), 150);
        pulse.kick();
        assert!(watch.observe(pulse.tick(), 5000));
    }

    #[test]
    fn pulse_stays_eight_bytes() {
        assert_eq!(core::mem::size_of::<WorkerPulse>(), 8);
    }
}
