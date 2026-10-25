//! Pool storm: N lanes × K hard process kills through ONE pool process.
//!
//! Builds `examples/19_worker_pool`, lets it spawn / kill / respawn every
//! lane itself (the app owns `spawn` and `kill`), then gates the JSON report:
//!
//! * `reaped == lanes × kills` — every child was accounted for;
//! * `ambush == reaped` — every kill landed after the arm proof;
//! * `deadlock == false` / `conserved == true` — no pipe was left in flight;
//! * `final_generation == [kills + 1; lanes]` — one bump per death, per lane.

mod common;

use common::json_num;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// A storm below the floor proves nothing: zero or one iteration always
/// "passes" the conservation and reaping gates vacuously.
fn env_num_min(name: &str, default: u64, min: u64) -> u64 {
    let value = env_num(name, default);
    assert!(
        value >= min,
        "{name}={value} is below the stress floor {min}: a storm that small proves nothing"
    );
    value
}

#[test]
#[ignore = "spawns lanes x kills real processes and hard-kills them; run explicitly: cargo test --test supervisor_pool_stress -- --ignored --nocapture"]
fn worker_pool_storm() {
    let report_path = "target/pool_stress_report.json";
    let _ = fs::remove_file(report_path);

    let status = Command::new("cargo")
        .args(["build", "--example", "19_worker_pool"])
        .status()
        .expect("cargo build --example 19_worker_pool");
    assert!(status.success(), "failed to build 19_worker_pool");

    let lanes = env_num_min("POOL_STRESS_LANES", 4, 2);
    let kills = env_num_min("POOL_STRESS_KILLS", 8, 2);
    let timeout_secs = (lanes * kills * 2).max(60);

    println!(
        "[TEST RUNNER] Pool storm: lanes={lanes} kills_per_lane={kills} (target {} kills)",
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
        "pool example exited with {status:?} — report: {content}"
    );
    assert!(
        !content.is_empty(),
        "no pool report inside {timeout_secs}s — the pool deadlocked"
    );

    println!("[TEST RUNNER] Report: {content}");

    let expected = lanes * kills;
    assert!(
        content.contains("\"deadlock\":false"),
        "pool reported a deadlocked lane: {content}"
    );
    assert!(
        content.contains("\"conserved\":true"),
        "pool accounting does not conserve: {content}"
    );
    assert_eq!(
        json_num(&content, "reaped"),
        expected,
        "expected {expected} reaps: {content}"
    );
    assert_eq!(
        json_num(&content, "ambush"),
        expected,
        "every kill must land after the arm proof: {content}"
    );
    assert_eq!(
        json_num(&content, "rescued"),
        expected,
        "every armed worker must hand its token back: {content}"
    );
    // Independent of the pool's own `rescued` counter: the app thread must
    // actually RECEIVE one token per ambush, and every token must name its
    // own lane (a token delivered to the wrong lineage is a routing bug the
    // counters cannot see).
    assert_eq!(
        json_num(&content, "tokens_seen"),
        expected,
        "app must receive one rescued token per ambush: {content}"
    );
    assert_eq!(
        json_num(&content, "token_lane_mismatch"),
        0,
        "a rescued token named the wrong lane: {content}"
    );
    // Bookkeeping cross-check: the per-class buckets must add up to the
    // reap total. A lost or double-counted reap shows here even when every
    // individual field looks plausible on its own.
    assert_eq!(
        json_num(&content, "ambush")
            + json_num(&content, "escape")
            + json_num(&content, "stillborn"),
        json_num(&content, "reaped"),
        "class buckets must sum to reaped: {content}"
    );
    assert_eq!(
        json_num(&content, "deadlocks"),
        0,
        "no lane may deadlock: {content}"
    );

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
        "[TEST RUNNER] SUCCESS: {lanes} lanes reaped {expected} armed workers, zero deadlocks"
    );
}
