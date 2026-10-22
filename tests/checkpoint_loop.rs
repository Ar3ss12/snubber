//! End-to-end checkpoint physics: a real supervisor, a real worker, a real
//! 16 MiB body on disk.
//!
//! * streaming — soft drain, `.tmp` + `sync_all` + `rename`, 64 B ticket,
//!   successor validates and reports `restored_bytes=16777216`;
//! * naive — `to_vec()` before the write crosses the hard limit, the
//!   supervisor kills mid-`write_all`, and only an unsealed `.tmp` survives.
//!
//! Skipped under Miri (OS process spawning is unsupported).

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
        .join("16_streaming_checkpoint");
    let bin = if cfg!(windows) {
        bin.with_extension("exe")
    } else {
        bin
    };
    if !bin.exists() {
        let mut build = Command::new("cargo");
        build.args(["build", "--example", "16_streaming_checkpoint"]);
        if profile == "release" {
            build.arg("--release");
        }
        let status = build.status().expect("build example");
        assert!(status.success(), "build 16_streaming_checkpoint");
    }
    assert!(bin.exists(), "example built: {bin:?}");
    bin
}

fn work_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("snubber_ckpt_loop_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn run_supervisor(bin: &Path, dir: &Path, extra: &[&str]) -> (String, i32) {
    let mut child = Command::new(bin)
        .arg("--supervisor")
        .arg("--work-dir")
        .arg(dir)
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn supervisor");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("supervisor did not terminate within 60 s");
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
fn streaming_checkpoint_seals_and_the_successor_restores() {
    let bin = example_bin();
    let dir = work_dir("stream");
    let (log, code) = run_supervisor(&bin, &dir, &[]);

    assert!(log.contains("[SUPERVISOR:MEMORY_DRAIN]"), "log:\n{log}");
    assert!(log.contains("used_kb=51200"), "soft sample:\n{log}");
    assert!(
        !log.contains("[SUPERVISOR:MEMORY_LIMIT]"),
        "64 KiB must fit in the headroom:\n{log}"
    );
    assert!(
        log.contains("[VERIFY] mode=streaming sealed=true tmp=false ticket=true successor=0"),
        "sealed file + ticket + clean successor:\n{log}"
    );
    assert!(
        log.contains("[SUCCESSOR] restored_bytes=16777216"),
        "successor restored the body through the 64 B ticket:\n{log}"
    );
    assert!(!log.contains("dead_ticket"), "ticket must be valid:\n{log}");
    assert_eq!(code, 0, "supervisor reports success");

    assert!(dir.join("state.sncb").is_file());
    assert!(!dir.join("state.tmp").exists(), "tmp renamed away");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "Miri does not support spawning OS processes or examples"
)]
fn naive_clone_is_killed_before_the_rename() {
    let bin = example_bin();
    let dir = work_dir("naive");
    let (log, code) = run_supervisor(&bin, &dir, &["--naive"]);

    let drain_at = log
        .find("[SUPERVISOR:MEMORY_DRAIN]")
        .unwrap_or_else(|| panic!("soft window observed first:\n{log}"));
    let kill_at = log
        .find("[SUPERVISOR:MEMORY_LIMIT]")
        .unwrap_or_else(|| panic!("hard kill missing:\n{log}"));
    assert!(drain_at < kill_at, "drain precedes euthanasia:\n{log}");
    assert!(
        log.contains("used_kb=67584") && log.contains("hard_kb=57344"),
        "second copy crossed hard:\n{log}"
    );
    assert!(log.contains("[SUPERVISOR:KILLED]"), "child reaped");
    assert!(
        log.contains("cause=\"process exceeded hard memory limit\""),
        "DeathCause::MemoryLimit stamped:\n{log}"
    );
    assert!(
        log.contains("[VERIFY] mode=naive sealed=false tmp=true ticket=false successor=skipped"),
        "no sealed file, no ticket, no successor:\n{log}"
    );
    assert!(
        !log.contains("restored_bytes"),
        "nothing to restore:\n{log}"
    );
    assert_eq!(code, 0, "supervisor reports success");

    assert!(!dir.join("state.sncb").exists(), "rename never ran");
    assert!(dir.join("state.tmp").is_file(), "killed mid-write");
    let _ = std::fs::remove_dir_all(&dir);
}
