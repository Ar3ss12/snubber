//! Round-trip for `take_stdin_digest` (Phase 3 stdin state handoff).
//!
//! The parent test re-executes THIS test binary with a piped stdin and a
//! report path in the env; the child test calls `take_stdin_digest()` on
//! startup and writes the digest CRC (or `NONE`) to the report file. This
//! is the only way to drive real stdin on both Windows and POSIX without
//! spawning `cargo` at test time.
//!
//! Contract pinned here: exactly `STATE_LEN` (64) bytes delivered ->
//! `Some` bit-exact digest; fewer bytes (torn write) -> `None`, never a
//! garbage digest.

use snubber::{StateDigest, STATE_LEN};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn report_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "snub_stdin_digest_{}_{}.txt",
        tag,
        std::process::id()
    ));
    p
}

#[test]
fn parent_pipes_digest_and_torn_write() {
    if std::env::var_os("SNUB_STDIN_CHILD").is_some() {
        return; // child re-entry: only `child_reports_digest` acts
    }
    let exe = std::env::current_exe().expect("test exe path");

    // 1. Full handoff: exactly 64 bytes -> Some, bit-exact (CRC compare).
    let out = report_path("full");
    let _ = std::fs::remove_file(&out);
    let mut child = Command::new(&exe)
        .env("SNUB_STDIN_CHILD", "1")
        .env("SNUB_STDIN_OUT", &out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("child test binary spawns");
    let payload = [0xA5u8; STATE_LEN];
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(&payload)
        .expect("64 B written");
    drop(child.stdin.take());
    let status = child.wait().expect("child exits");
    assert!(status.success(), "child test run must pass");
    let text = std::fs::read_to_string(&out).expect("child report file");
    let _ = std::fs::remove_file(&out);
    let expected = StateDigest::new(payload);
    assert_eq!(
        text.trim(),
        format!("OK {}", expected.crc()),
        "full stdin handoff must round-trip bit-exact"
    );

    // 2. Torn write: 30 of 64 bytes -> None (reported as NONE), the
    // replacement process must not adopt a garbage digest.
    let out = report_path("torn");
    let _ = std::fs::remove_file(&out);
    let mut child = Command::new(&exe)
        .env("SNUB_STDIN_CHILD", "1")
        .env("SNUB_STDIN_OUT", &out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("child test binary spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(&[0x5A_u8; 30])
        .expect("30 B written");
    drop(child.stdin.take());
    let status = child.wait().expect("child exits");
    assert!(status.success(), "child test run must pass");
    let text = std::fs::read_to_string(&out).expect("child report file");
    let _ = std::fs::remove_file(&out);
    assert_eq!(text.trim(), "NONE", "torn write must yield None");
}

#[test]
fn child_reports_digest() {
    let out = match std::env::var_os("SNUB_STDIN_OUT") {
        Some(p) => PathBuf::from(p),
        None => return, // parent run: nothing to report
    };
    let reported = match snubber::take_stdin_digest() {
        Some(d) => format!("OK {}", d.crc()),
        None => "NONE".to_string(),
    };
    std::fs::write(&out, reported).expect("report written");
}
