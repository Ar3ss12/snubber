//! Mixed-class pool storm: four death shapes in ONE run.
//!
//! `examples/19_worker_pool --mixed` cycles each lane through
//! `armed → clean → silent → orphan` by `local % 4`:
//!
//! * `armed`   — arm frame parsed, then killed from outside → Ambush + token;
//! * `clean`   — arm + commit parsed, then killed → Escape;
//! * `silent`  — no frame at all, exited/killed → Stillborn;
//! * `orphan`  — arm parsed, but a grandchild holds the pipe open → Deadlocked.
//!
//! A healthy storm therefore reports an EQUAL four-way split, conserves
//! every reap, bumped each lane exactly once per death, and marks the run
//! `deadlock: true` (the honest bucket, not a lost token).

use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn per_lane(kills: u64, slot: u64) -> u64 {
    (kills + (3 - slot)) / 4
}

#[test]
#[ignore = "spawns lanes x kills real processes, leaves an orphan pipe holder per orphan cycle; run explicitly: cargo test --test supervisor_pool_stress_mixed -- --ignored --nocapture"]
fn worker_pool_mixed_storm() {
    let report_path = "target/pool_mixed_stress_report.json";
    let _ = fs::remove_file(report_path);

    let status = Command::new("cargo")
        .args(["build", "--example", "19_worker_pool"])
        .status()
        .expect("cargo build --example 19_worker_pool");
    assert!(status.success(), "failed to build 19_worker_pool");

    let lanes = env_num("POOL_MIXED_LANES", 4);
    let kills = env_num("POOL_MIXED_KILLS", 8);
    let timeout_secs = (lanes * kills * 2).max(60);

    println!(
        "[TEST RUNNER] Mixed pool storm: lanes={lanes} kills_per_lane={kills} (target {} reaps)",
        lanes * kills
    );

    let mut run = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--example",
            "19_worker_pool",
            "--",
            "--lanes",
            &lanes.to_string(),
            "--kills",
            &kills.to_string(),
            "--mixed",
            "--grace-ms",
            "150",
            "--report",
            report_path,
        ])
        .spawn()
        .expect("spawn pool example");

    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let mut content = String::new();
    while start.elapsed() < timeout {
        if let Ok(text) = fs::read_to_string(report_path) {
            if !text.trim().is_empty() {
                content = text;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let status = run.wait().expect("wait pool example");
    assert!(
        status.success(),
        "mixed pool example exited with {status:?} — report: {content}"
    );
    assert!(
        !content.is_empty(),
        "no mixed pool report inside {timeout_secs}s"
    );

    println!("[TEST RUNNER] Report: {content}");

    let expected = lanes * kills;
    let want_ambush = lanes * per_lane(kills, 0);
    let want_escape = lanes * per_lane(kills, 1);
    let want_stillborn = lanes * per_lane(kills, 2);
    let want_deadlocked = lanes * per_lane(kills, 3);

    assert!(
        content.contains("\"conserved\":true"),
        "mixed accounting does not conserve: {content}"
    );
    assert!(
        content.contains("\"deadlock\":true"),
        "the orphan mode must surface as a deadlock bucket: {content}"
    );
    assert!(
        content.contains(&format!("\"reaped\":{expected}")),
        "expected {expected} reaps: {content}"
    );
    for (needle, want) in [
        ("ambush", want_ambush),
        ("escape", want_escape),
        ("stillborn", want_stillborn),
        ("deadlocks", want_deadlocked),
        ("rescued", want_ambush),
    ] {
        assert!(
            content.contains(&format!("\"{needle}\":{want}")),
            "expected {needle}={want}: {content}"
        );
    }

    let generations = std::iter::repeat(kills + 1)
        .take(lanes as usize)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        content.contains(&format!("\"final_generation\":[{generations}]")),
        "each lane must bump once per death (want [{generations}]): {content}"
    );

    println!(
        "[TEST RUNNER] SUCCESS: {lanes} lanes reaped {expected} mixed deaths — A/E/S/D={want_ambush}/{want_escape}/{want_stillborn}/{want_deadlocked}, zero lost tokens"
    );
}
