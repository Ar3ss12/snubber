//! Pattern 15: Supervised Memory Loop (soft drain vs hard kill).
//!
//! One binary, two roles — the same shape as `process_termination_loop`:
//!
//! * `--supervisor` — samples the child's RSS through the APP-owned
//!   `sample(pid)` closure and runs
//!   [`Supervisor::run_supervised_guard_with_memory`]. Soft only logs
//!   `[SUPERVISOR:MEMORY_DRAIN]`; hard kills without draining.
//! * `--worker` — a `SnubberNode` with the same limits. Under `--drain`
//!   (default) it stops admitting at soft, emits a 64 B `StateDigest`
//!   and exits for a successor generation. Under `--no-drain` it ignores
//!   the soft limit, so the supervisor has to euthanize it at hard.
//!
//! The sampler here reads a logical usage file the worker writes, so the
//! demo needs no platform RSS call (the crate measures nothing). In
//! production `sample` reads `/proc/<pid>/status`, `GetProcessMemoryInfo`
//! or the cgroup — same signature, no `unsafe` in the crate.
//!
//! Run with:
//! ```powershell
//! cargo run --example 15_supervised_memory -- --supervisor
//! cargo run --example 15_supervised_memory -- --supervisor --no-drain
//! ```

use snubber::{MemoryPolicy, MemoryVerdict, SnubberNode, StateDigest, Supervisor, STATE_LEN};
use std::time::Duration;

const SOFT_KB: u64 = 64_000;
const HARD_KB: u64 = 96_000;
const START_KB: u64 = 40_000;
const STEP_KB: u64 = 10_000;

fn main() {
    println!("=== Pattern 15: Supervised Memory Loop ===");
    let args: Vec<String> = std::env::args().collect();
    let usage_file = arg_value(&args, "--usage-file")
        .unwrap_or_else(|| "target/supervised_memory_usage_kb".to_string());

    if args.iter().any(|a| a == "--worker") {
        worker(&usage_file, !args.iter().any(|a| a == "--no-drain"));
        return;
    }

    if !args.iter().any(|a| a == "--supervisor") {
        println!("usage: 15_supervised_memory --supervisor [--no-drain] [--usage-file PATH]");
        return;
    }

    // Stale reading from a previous generation must never kill the new
    // child before it publishes its first sample.
    let _ = std::fs::remove_file(&usage_file);

    let policy = MemoryPolicy::new(SOFT_KB, HARD_KB);
    let sample = move |_pid: u32| {
        std::fs::read_to_string(&usage_file)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    Supervisor::run_supervised_guard_with_memory("--worker", 0, policy, sample, |token| {
        println!(
            "[APP] rescued armed token task={} opcode={}",
            token.task, token.opcode
        );
    });
}

fn worker(usage_file: &str, drain: bool) {
    let node = SnubberNode::builder("memory/supervised-loop")
        .memory_limits(SOFT_KB, HARD_KB)
        .build();

    let mut usage_kb = START_KB;
    for tick in 0..40u64 {
        // The APP publishes the reading; the supervisor's sampler reads it.
        let _ = std::fs::write(usage_file, usage_kb.to_string());
        let verdict = node.note_memory_kb(usage_kb);

        match node.enter(tick + 1, tick, || {}) {
            Ok(guard) => {
                guard.commit();
            }
            Err(e) => println!("[WORKER] tick={tick} refused: {e:?}"),
        }

        if verdict == MemoryVerdict::Soft {
            if drain {
                let mut bytes = [0u8; STATE_LEN];
                bytes[0] = tick as u8;
                bytes[1] = 1; // "drained, not killed"
                let digest = StateDigest::new(bytes);
                digest.emit_if_supervised();
                println!(
                    "[WORKER] soft at {usage_kb} KiB — digest crc=0x{:08X}, draining in-flight work",
                    digest.crc()
                );
                // Drain window: in-flight guards settle while the supervisor
                // observes the soft sample; then exit for a successor.
                std::thread::sleep(Duration::from_millis(250));
                std::process::exit(0);
            }
            // `--no-drain`: keep the process alive past soft so the
            // supervisor has to euthanize it at hard.
            std::thread::sleep(Duration::from_millis(250));
        }

        std::thread::sleep(Duration::from_millis(50));
        usage_kb += STEP_KB;
    }
    std::process::exit(0);
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
