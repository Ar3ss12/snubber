//! Automated system kill stress test (Hard process termination harness).
//! Spawns Supervisor and Stress Killer Harness,
//! verifies that hard process terminations (TerminateProcess on Windows, SIGKILL on Unix)
//! are survived with 0 pipe deadlocks and 100% token accounting.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
#[ignore = "Spawns background supervisor & killer and executes hundreds/thousands of hard OS kills. Run explicitly: cargo test --test chaos_dual_terminal -- --ignored --nocapture"]
fn chaos_dual_terminal_system_kill_test() {
    let report_file = "target/chaos_report.json";
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

    let kills: u32 = std::env::var("CHAOS_KILLS")
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
        // 1. Spawn Terminal 1: Supervisor Instance
        let cmd1 = format!(
            "$host.UI.RawUI.WindowTitle = 'SNUBBER SUPERVISOR (TERMINAL 1)'; cargo run --example process_termination_loop -- --target-kills {} --report-path {}",
            kills, report_file
        );
        let mut term1 = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Start-Process powershell -ArgumentList '-NoExit', '-Command', \"{}\"",
                    cmd1
                ),
            ])
            .spawn()
            .expect("spawn terminal 1");
        let _ = term1.wait();

        std::thread::sleep(Duration::from_millis(1500));

        // 2. Spawn Terminal 2: Process Killer Harness
        let cmd2 = format!(
            "powershell -ExecutionPolicy Bypass -File .\\stress_harness.ps1 -TargetProcess process_termination_loop -Kills {}",
            kills
        );
        let mut term2 = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Start-Process powershell -ArgumentList '-NoExit', '-Command', \"{}\"",
                    cmd2
                ),
            ])
            .spawn()
            .expect("spawn terminal 2");
        let _ = term2.wait();
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
                "stress_harness.sh",
                "process_termination_loop",
                &kills.to_string(),
                "0.001",
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
                if f.read_to_string(&mut report_content).is_ok() && !report_content.is_empty() {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(
        !report_content.is_empty(),
        "Timeout waiting for chaos test report! Pipe might have deadlocked."
    );

    println!("[TEST RUNNER] Report received:\n{}", report_content);

    // Verify properties
    assert!(
        report_content.contains("\"deadlock\": false"),
        "Supervisor reported deadlock!"
    );
    assert!(
        report_content.contains(&format!("\"total_reaped\": {}", kills)),
        "Total reaped count mismatch!"
    );
    assert!(
        report_content.contains(&format!("\"final_generation\": {}", kills + 1)),
        "Final generation mismatch! Expected generation = kills + 1"
    );

    println!(
        "[TEST RUNNER] SUCCESS: All {} system-level executions survived perfectly!",
        kills
    );
}
