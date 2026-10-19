//! Pattern 11: Hang classifier (SPIN vs SLEEP) with an app-side CPU clock.
//!
//! The crate holds only the `CpuClock` trait + `classify_stall` math
//! (`core`, no `unsafe`). The backend below is APPLICATION code: it
//! samples a thread CPU counter next to the wall tick and arms the
//! guard with `with_cpu_sampler`. A hot `loop {}` settles as
//! `SpinDetected`; a blocked syscall settles as plain `TimedOut`.
//!
//! Run: `cargo run --example 11_hang_classifier`

use snubber::{SnubberNode, TimeoutCheck};
use std::sync::atomic::{AtomicU64, Ordering};

/// Simulated OS thread-CPU counter (stand-in for `GetThreadTimes` on
/// Windows / `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` on Linux).
static SIM_CPU_MS: AtomicU64 = AtomicU64::new(0);

/// App-side sampler: plain `fn() -> u64`, no closure, no lock.
fn thread_cpu_ms() -> u64 {
    SIM_CPU_MS.load(Ordering::SeqCst)
}

fn main() {
    println!("=== Pattern 11: Hang classifier (SPIN vs SLEEP) ===");

    // Scenario A: hot spin — CPU grows with the wall.
    // Arming sample is taken inside `with_cpu_sampler`, so the budget
    // must be consumed BETWEEN arming and the check: each loop step
    // advances the simulated CPU counter past `cpu_budget_ms`.
    println!("\nScenario A: hot spin (CPU + wall grow together)");
    SIM_CPU_MS.store(0, Ordering::SeqCst);
    let node = SnubberNode::builder("hang/spin")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .build();
    let guard = node
        .enter(1, 1, || {
            println!("[Guard] spin rollback: iron de-energized")
        })
        .expect("admit")
        .with_cpu_sampler(thread_cpu_ms);
    // Burn CPU the way `loop {}` would: consume the whole CPU budget
    // between arming and the check (delta >= cpu_budget_ms = spin).
    // NOTE: no early break — the wall lease must ALSO blow (1 ms), and
    // `stall_kind` turns non-budget only after BOTH grow together.
    for _ in 0..60 {
        SIM_CPU_MS.fetch_add(10, Ordering::SeqCst);
        std::thread::sleep(core::time::Duration::from_millis(1));
    }
    match guard.check_timeout() {
        Err(TimeoutCheck::SpinDetected) => println!("[Watch] SPIN detected: core hot, no progress"),
        Err(_) => panic!("spin must settle as SpinDetected"),
        Ok(_) => panic!("spin must not stay within budget"),
    }

    // Scenario B: blocked sleep — wall grows, CPU stands still.
    println!("\nScenario B: blocked sleep (wall grows, CPU frozen)");
    SIM_CPU_MS.store(0, Ordering::SeqCst);
    let node = SnubberNode::builder("hang/sleep")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .build();
    let guard = node
        .enter(2, 2, || {
            println!("[Guard] sleep rollback: iron de-energized")
        })
        .expect("admit")
        .with_cpu_sampler(thread_cpu_ms);
    std::thread::sleep(core::time::Duration::from_millis(10));
    match guard.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {
            println!("[Watch] SLEEP timeout: thread blocked, not spinning")
        }
        Err(_) => panic!("sleep must settle as TimedOut, not SpinDetected"),
        Ok(_) => panic!("sleep must not stay within budget"),
    }

    println!("\nVerification: SPIN vs SLEEP classified without killing any thread.");
}
