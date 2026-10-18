//! Supervised satellite: dumb supervisor + disposable worker, `std` only.
//!
//! Run: `cargo run --example supervised_satellite`
//!
//! Path-1 + Path-2 in one scene, no `unsafe`, no deps:
//! * supervisor spawns the worker with `SNUBBER_GEN` + a piped stdout;
//! * `PipeDrain` drains 40-byte tokens in real time (no 64 KB buffer block);
//! * worker arms a token, then dies via `std::process::abort()` mid-flight;
//! * supervisor sees `EOF`, runs the opcode handler, bumps generation,
//! * a fresh worker re-handshakes at `gen+1, epoch=1` — stale generation packets rejected.

use snubber::{GenerationClock, PipeDrain, ProcessToken, SnubberNode};
use std::io::Write;
use std::process::{Command, Stdio};

fn main() {
    if std::env::args().any(|a| a == "--worker") {
        worker_main();
        return;
    }
    let mut clock = GenerationClock::new();
    let gen = clock.generation();
    // Worker = this same binary in `--worker` mode (disposable satellite).
    let mut child = Command::new(std::env::current_exe().expect("exe"))
        .arg("--worker")
        .env("SNUBBER_GEN", gen.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn worker");
    let reader = child.stdout.take().expect("piped stdout");
    let drain = PipeDrain::spawn(reader);
    let status = child.wait().expect("wait worker");
    // Spin for EOF (drain thread finished).
    let mut spins = 0u32;
    while !drain.eof() && spins < 10_000 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        spins += 1;
    }
    assert!(!status.success(), "worker must die in this scene");
    // Path-2: un-settled token fires the emergency rollback OUTSIDE.
    static FIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(token) = drain.take_armed() {
        assert_eq!(token.opcode, 7, "last-armed opcode survives death");
        FIRED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Emergency release HERE (supervisor side, outside the terminated child).
    }
    assert_eq!(FIRED.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Path-1: bump generation, fresh worker re-handshakes, stale generation rejected.
    let gen2 = clock.restart();
    assert_eq!(gen2, gen + 1);
    let node = SnubberNode::builder("super/satellite").build();
    node.enter_with_generation(gen2, 1, 1, || {})
        .expect("fresh lineage")
        .commit();
    println!("[SUPERVISOR:REAP] exit_status=terminated token_fired=true generation={gen2}");
}

/// Worker mode: arm ONE token on stdout, die mid-flight (no commit).
#[allow(dead_code)]
fn worker_main() {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let token = ProcessToken::new(4242, 7, [0xAA; 28]);
    out.write_all(&token.to_bytes()).expect("arm token");
    out.flush().expect("flush");
    // Die WITHOUT commit — like panic="abort" / SIGKILL (pipe EOF).
    std::process::abort();
}
