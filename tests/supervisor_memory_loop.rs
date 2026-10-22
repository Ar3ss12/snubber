//! End-to-end supervised memory loop: a real supervisor process spawns a
//! real worker, samples its usage through the APP-owned sampler and either
//! observes a soft drain or euthanizes the child at the hard limit.
//!
//! Drives `examples/15_supervised_memory.rs`; the crate itself never reads
//! RSS, so the example's sampler reads a logical usage file the worker
//! publishes. Skipped under Miri (OS process spawning is unsupported).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn example_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("exe");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let bin = exe
        .parent()
        .expect("deps dir")
        .parent()
        .expect("profile dir")
        .join("examples")
        .join("15_supervised_memory");
    let bin = if cfg!(windows) {
        bin.with_extension("exe")
    } else {
        bin
    };
    if !bin.exists() {
        let mut build = Command::new("cargo");
        build.args(["build", "--example", "15_supervised_memory"]);
        if profile == "release" {
            build.arg("--release");
        }
        let status = build.status().expect("build example");
        assert!(status.success(), "build 15_supervised_memory");
    }
    assert!(bin.exists(), "example built: {bin:?}");
    bin
}

fn usage_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("snubber_mem_loop_{tag}_{}.kb", std::process::id()))
}

fn run_supervisor(bin: &Path, usage: &Path, extra: &[&str]) -> (String, i32) {
    let mut child = Command::new(bin)
        .arg("--supervisor")
        .arg("--usage-file")
        .arg(usage)
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn supervisor");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("supervisor did not terminate within 30 s");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let out = child.wait_with_output().expect("collect output");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.code().expect("exit code"))
}

#[test]
#[cfg_attr(
    miri,
    ignore = "Miri does not support spawning OS processes or examples"
)]
fn soft_limit_drains_and_exits_clean() {
    let bin = example_bin();
    let usage = usage_path("drain");
    let (log, code) = run_supervisor(&bin, &usage, &[]);

    assert!(log.contains("[SUPERVISOR:MEMORY_DRAIN]"), "log:\n{log}");
    assert!(log.contains("used_kb=70000"), "sampler saw soft: \n{log}");
    assert!(
        log.contains("[SUPERVISOR:TERMINATED]"),
        "worker exits itself"
    );
    assert!(log.contains("[SUPERVISOR:CLEAN]"), "no token left armed");
    assert!(
        !log.contains("[SUPERVISOR:MEMORY_LIMIT]"),
        "soft must never kill:\n{log}"
    );
    assert_eq!(code, 0, "supervisor reports success");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "Miri does not support spawning OS processes or examples"
)]
fn hard_limit_kills_without_draining() {
    let bin = example_bin();
    let usage = usage_path("kill");
    let (log, code) = run_supervisor(&bin, &usage, &["--no-drain"]);

    let drain_at = log
        .find("[SUPERVISOR:MEMORY_DRAIN]")
        .unwrap_or_else(|| panic!("soft window observed first:\n{log}"));
    let kill_at = log
        .find("[SUPERVISOR:MEMORY_LIMIT]")
        .unwrap_or_else(|| panic!("hard kill missing:\n{log}"));
    assert!(drain_at < kill_at, "drain precedes euthanasia:\n{log}");
    assert!(
        log.contains("used_kb=100000") && log.contains("hard_kb=96000"),
        "hard sample logged with limits:\n{log}"
    );
    assert!(log.contains("[SUPERVISOR:KILLED]"), "child reaped");
    assert!(
        log.contains("cause=\"process exceeded hard memory limit\""),
        "DeathCause::MemoryLimit stamped:\n{log}"
    );
    assert_eq!(code, 0, "supervisor reports success");
}
