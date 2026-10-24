//! Lease-euthanasia pool storm: N lanes, silent children, one live observer.
//!
//! `examples/19_worker_pool --lease-ms 25` makes every child send exactly
//! ONE heartbeat and then go silent. A lane waiter blocks on the lease:
//! once `PoolLane::expired(25)` flips, it kills the child from outside and
//! reaps it. At the same time a monitor thread keeps calling
//! `pool.report()` + `WorkerPool::expired_lanes(25)`, so a torn accounting
//! window (a reap counted without its class, or a bogus lane index) would
//! be observed DURING the storm, not inferred from the final card.
//!
//! Expected: every reap is `Stillborn` (a heartbeat proves the pipe, not a
//! settlement), `lease_kills == reaps`, `conservation_breaks == 0`,
//! `monitor_checks > 0`, no deadlocks, one generation bump per death.

use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn json_num(content: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\":");
    let start = content
        .find(&needle)
        .unwrap_or_else(|| panic!("missing {key}: {content}"))
        + needle.len();
    content[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number: {content}"))
}

#[test]
#[ignore = "spawns lanes x kills real silent processes on a 25 ms lease; run explicitly: cargo test --test supervisor_pool_stress_lease -- --ignored --nocapture"]
fn worker_pool_lease_storm() {
    let report_path = "target/pool_lease_stress_report.json";
    let _ = fs::remove_file(report_path);

    let status = Command::new("cargo")
        .args(["build", "--example", "19_worker_pool"])
        .status()
        .expect("cargo build --example 19_worker_pool");
    assert!(status.success(), "failed to build 19_worker_pool");

    let lanes = env_num("POOL_LEASE_LANES", 4);
    let kills = env_num("POOL_LEASE_KILLS", 6);
    let lease_ms = env_num("POOL_LEASE_MS", 25);
    let timeout_secs = (lanes * kills * 2).max(60);

    println!("[TEST RUNNER] Lease storm: lanes={lanes} kills_per_lane={kills} lease={lease_ms} ms");

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
            "--lease-ms",
            &lease_ms.to_string(),
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
        "lease pool example exited with {status:?} — report: {content}"
    );
    assert!(
        !content.is_empty(),
        "no lease pool report inside {timeout_secs}s"
    );

    println!("[TEST RUNNER] Report: {content}");

    let expected = lanes * kills;
    assert!(
        content.contains("\"conserved\":true"),
        "lease accounting does not conserve: {content}"
    );
    assert!(
        content.contains("\"deadlock\":false"),
        "a killed-after-lease child must still reach EOF: {content}"
    );
    // Read the actual numbers: a substring gate would accept `"reaped":40`
    // for an expected 4 (value-prefix collision) — the overcount class this
    // stress exists to catch.
    assert_eq!(
        json_num(&content, "reaped"),
        expected,
        "expected {expected} reaps: {content}"
    );
    assert_eq!(
        json_num(&content, "stillborn"),
        expected,
        "heartbeats are not settlements — every reap is stillborn: {content}"
    );
    for zero in [
        "rescued",
        "ambush",
        "escape",
        "deadlocks",
        "conservation_breaks",
    ] {
        assert_eq!(json_num(&content, zero), 0, "expected {zero}=0: {content}");
    }
    // Bookkeeping cross-check: the per-class buckets must add up to reaped.
    assert_eq!(
        json_num(&content, "ambush")
            + json_num(&content, "escape")
            + json_num(&content, "stillborn"),
        json_num(&content, "reaped"),
        "class buckets must sum to reaped: {content}"
    );
    assert_eq!(
        json_num(&content, "lease_kills"),
        expected,
        "every child must be euthanized BY the lease: {content}"
    );
    let checks = json_num(&content, "monitor_checks");
    assert!(
        checks > 0,
        "the live observer never sampled the pool: {content}"
    );
    assert_eq!(
        json_num(&content, "tokens_seen"),
        0,
        "a heartbeat-only child has no settlement to rescue: {content}"
    );
    assert_eq!(
        json_num(&content, "token_lane_mismatch"),
        0,
        "no token may cross lanes: {content}"
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
        "[TEST RUNNER] SUCCESS: {lanes} lanes euthanized {expected} silent workers by lease — {checks} live report samples, 0 conservation breaks"
    );
}
