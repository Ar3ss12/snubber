//! Automated system kill stress test (Hard process termination harness).
//! Spawns Supervisor and Stress Killer Harness,
//! verifies that hard process terminations (TerminateProcess on Windows, SIGKILL on Unix)
//! are survived with 0 pipe deadlocks and 100% token accounting.

mod common;

use common::json_num;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
#[ignore = "Spawns background supervisor & killer and executes hundreds/thousands of hard OS kills. Run explicitly: cargo test --test supervisor_termination -- --ignored --nocapture"]
fn process_kill_stress_test() {
    let report_file = "target/termination_report.json";
    if Path::new(report_file).exists() {
        let _ = std::fs::remove_file(report_file);
    }

    // Ensure the process_termination_loop example is compiled
    let status = Command::new("cargo")
        .args(["build", "--example", "process_termination_loop"])
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
        "[TEST RUNNER] Launching supervisor and termination harness (target kills: {})...",
        kills
    );

    #[cfg(windows)]
    {
        // 1. Spawn Supervisor Instance in background (headless, no popup window)
        let mut supervisor = Command::new("cargo")
            .args([
                "run",
                "--example",
                "process_termination_loop",
                "--",
                "--target-kills",
                &kills.to_string(),
                "--report-path",
                report_file,
            ])
            .spawn()
            .expect("spawn supervisor");

        std::thread::sleep(Duration::from_millis(1500));

        // 2. Spawn Process Killer Harness in background
        let mut killer = Command::new("powershell")
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
            ])
            .spawn()
            .expect("spawn killer harness");

        let _ = killer.wait();
        let _ = supervisor.wait();
    }

    #[cfg(not(windows))]
    {
        // 1. Spawn Supervisor Instance in background
        let mut supervisor = Command::new("cargo")
            .args([
                "run",
                "--example",
                "process_termination_loop",
                "--",
                "--target-kills",
                &kills.to_string(),
                "--report-path",
                report_file,
            ])
            .spawn()
            .expect("spawn supervisor");

        std::thread::sleep(Duration::from_millis(1500));

        // 2. Run stress harness in background
        let mut killer = Command::new("bash")
            .args([
                "scripts/stress_harness.sh",
                "process_termination_loop",
                &kills.to_string(),
                "0.001",
                // The test owns the supervisor; the harness CI gate needs
                // to know this is deliberate, not a forgotten --launch.
                "--external-supervisor",
            ])
            .spawn()
            .expect("spawn stress harness");

        // Wait for processes
        let _ = killer.wait();
        let _ = supervisor.wait();
    }

    println!(
        "[TEST RUNNER] Both processes launched! Waiting for report file: {}",
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
    let settled =
        json_num(&report_content, "clean_settles") + json_num(&report_content, "ambush_kills");
    assert_eq!(
        settled, kills,
        "every reaped process must be classified (clean + ambush)"
    );
    let classes = json_num(&report_content, "ambush_kills")
        + json_num(&report_content, "escape_kills")
        + json_num(&report_content, "stillborn_kills");
    assert_eq!(classes, kills, "class buckets must sum to reaped");
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
