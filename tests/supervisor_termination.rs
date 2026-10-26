//! Automated system kill stress test (Hard process termination harness).
//! Spawns the stress harness, which launches the supervisor and hard-kills
//! its workers (TerminateProcess on Windows, SIGKILL on Unix), then verifies
//! that the terminations were survived with 0 pipe deadlocks and 100% token
//! accounting.

mod common;

use common::json_num;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// Wait on a child with a hard deadline. A harness that refuses to run (the
/// CI `--launch` gate) used to leave the caller blocked on `wait()` until the
/// job timeout; a wedged child now fails the lane in bounded time.
fn wait_bounded(child: &mut Child, limit: Duration, what: &str) -> ExitStatus {
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return status,
            None if start.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "{what} exceeded {limit:?}; killed so the lane fails fast instead of hanging"
                );
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[test]
#[ignore = "Spawns background supervisor & killer and executes hundreds/thousands of hard OS kills. Run explicitly: cargo test --test supervisor_termination -- --ignored --nocapture"]
fn process_kill_stress_test() {
    let report_file = "target/termination_report.json";
    if Path::new(report_file).exists() {
        let _ = std::fs::remove_file(report_file);
    }

    // Build in the profile the harness is told to launch (debug keeps this
    // lane cheap; the harness validates the binary exists before killing).
    let status = Command::new("cargo")
        .args(["build", "--locked", "--example", "process_termination_loop"])
        .status()
        .expect("cargo build --example process_termination_loop");
    assert!(
        status.success(),
        "failed to build process_termination_loop example"
    );

    let kills: u64 = std::env::var("STRESS_TARGET_KILLS")
        .or_else(|_| std::env::var("CHAOS_KILLS"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let timeout_secs = (kills as u64 * 300 / 1000).max(60);

    println!(
        "[TEST RUNNER] Launching stress harness (target kills: {})...",
        kills
    );

    // The harness owns the supervisor on EVERY platform: its RSS / handle /
    // fd / exit gates need the supervisor pid, and the harness refuses a
    // kill-only run under CI for exactly that reason. Launching the
    // supervisor here (the old shape) made the harness exit immediately and
    // left the supervisor waiting for kills that never came.
    #[cfg(windows)]
    let mut harness = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            ".\\scripts\\stress_harness.ps1",
            "-TargetProcess",
            "process_termination_loop",
            "-Kills",
            &kills.to_string(),
            "-Profile",
            "debug",
            "-ReportPath",
            report_file,
            "-Launch",
        ])
        .spawn()
        .expect("spawn stress harness");

    #[cfg(not(windows))]
    let mut harness = Command::new("bash")
        .args([
            "scripts/stress_harness.sh",
            "process_termination_loop",
            &kills.to_string(),
            "0.001",
            "--debug",
            "--launch",
        ])
        .spawn()
        .expect("spawn stress harness");

    let harness_status = wait_bounded(
        &mut harness,
        Duration::from_secs(timeout_secs.max(180)),
        "stress harness",
    );
    assert!(
        harness_status.success(),
        "stress harness failed ({harness_status}); see the [HARNESS] verdict above"
    );

    println!(
        "[TEST RUNNER] Harness finished. Waiting for report file: {}",
        report_file
    );

    // 3. Wait for the supervisor to complete and write the JSON report
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    let mut report_content = String::new();
    while start.elapsed() < timeout {
        if Path::new(report_file).exists() {
            std::thread::sleep(Duration::from_millis(200));
            if let Ok(mut f) = File::open(report_file) {
                // Require the closing brace: a torn first read must not
                // freeze a truncated JSON (fail-flake); keep polling
                // until the writer flushed the whole report.
                report_content.clear();
                if f.read_to_string(&mut report_content).is_ok()
                    && report_content.trim_end().ends_with('}')
                {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(
        !report_content.is_empty(),
        "Timeout waiting for process termination report! Pipe might have deadlocked."
    );

    println!("[TEST RUNNER] Report received:\n{}", report_content);

    // Verify properties. `reaped` must be partitioned across the three
    // termination classes — a silent unclassified death would break the sum.
    assert_eq!(
        json_num(&report_content, "total_reaped"),
        kills,
        "Total reaped count mismatch!"
    );
    let classes = json_num(&report_content, "ambush_kills")
        + json_num(&report_content, "escape_kills")
        + json_num(&report_content, "stillborn_kills");
    assert_eq!(classes, kills, "class buckets must sum to reaped");
    // Cross-source conservation (PLAN_AUDIT2 F-06): the class buckets are
    // derived in ONE if/else chain, so any sum of them is true by
    // construction. The children's own frame accounting — parsed from the
    // pipe into drain counters — is an independent record of what the
    // workers actually did:
    //   * an ambush required a parsed arm frame (the token came from it);
    //   * an escape required at least one parsed commit frame.
    // A misclassification (e.g. every death called `stillborn`) breaks
    // these floors even though every bucket sum stays green.
    let arms_sum = json_num(&report_content, "arms_sum");
    let commits_sum = json_num(&report_content, "commits_sum");
    let ambush = json_num(&report_content, "ambush_kills");
    let escape = json_num(&report_content, "escape_kills");
    assert!(
        arms_sum >= ambush + escape,
        "every ambush/escape cycle must have parsed an arm frame:          arms_sum={arms_sum} < ambush+escape={}",
        ambush + escape
    );
    assert!(
        commits_sum >= escape,
        "every escape cycle must have parsed a commit frame:          commits_sum={commits_sum} < escape={escape}"
    );
    assert_eq!(
        json_num(&report_content, "misclassified_stillborn"),
        0,
        "a Stillborn verdict contradicts observed commit frames —          the escape/stillborn split is broken"
    );
    if report_content.contains("\"armed_hold\": true") {
        assert_eq!(
            ambush, kills,
            "armed-hold run: every kill must be an ambush"
        );
    }
    assert_eq!(
        json_num(&report_content, "final_generation"),
        kills + 1,
        "Final generation mismatch! Expected generation = kills + 1"
    );

    println!(
        "[TEST RUNNER] SUCCESS: All {} system-level executions survived perfectly!",
        kills
    );
}
