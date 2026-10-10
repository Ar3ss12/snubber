//! Automated dual-terminal system kill test.
//! Spawns Terminal 1 (Supervisor Bunker) and Terminal 2 (Chaos Sniper),
//! verifies that 50 hard TerminateProcess kills are survived with 0 pipe deadlocks
//! and 100% token accounting.

#![cfg(windows)]

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
#[cfg(windows)]
#[ignore = "Spawns 2 real OS GUI windows and executes hundreds/thousands of TerminateProcess kills. Run explicitly: cargo test --test chaos_dual_terminal -- --ignored --nocapture"]
fn chaos_dual_terminal_system_kill_test() {
    let report_file = "target/chaos_report.json";
    if Path::new(report_file).exists() {
        let _ = std::fs::remove_file(report_file);
    }

    // Ensure the chaos_satellite example is compiled
    let status = Command::new("cargo")
        .args(["build", "--example", "chaos_satellite"])
        .status()
        .expect("cargo build --example chaos_satellite");
    assert!(status.success(), "failed to build chaos_satellite example");

    let kills: u32 = std::env::var("CHAOS_KILLS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let timeout_secs = (kills as u64 * 300 / 1000).max(60);

    println!(
        "[TEST RUNNER] Spawning dual terminals on Windows via PowerShell (kills: {})...",
        kills
    );

    // 1. Spawn Terminal 1: Supervisor Bunker
    let cmd1 = format!(
        "$host.UI.RawUI.WindowTitle = 'SNUBBER BUNKER (TERMINAL 1)'; cargo run --example chaos_satellite -- --target-kills {} --report-path {}",
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

    // Give the bunker a moment to start the supervisor loop
    std::thread::sleep(Duration::from_millis(1500));

    // 2. Spawn Terminal 2: Chaos Sniper
    let cmd2 = format!(
        "powershell -ExecutionPolicy Bypass -File .\\chaos_sniper.ps1 -TargetProcess chaos_satellite -Kills {}",
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

    println!(
        "[TEST RUNNER] Both terminals launched! Waiting for report file: {}",
        report_file
    );

    // 3. Wait for the bunker to complete and write the JSON report
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    let mut report_content = String::new();
    while start.elapsed() < timeout {
        if Path::new(report_file).exists() {
            // Wait an extra 200ms to ensure file write flush
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
