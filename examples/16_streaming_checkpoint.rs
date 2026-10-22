//! Pattern 16: Streaming Checkpoint — 64 KiB buffer vs naive clone.
//!
//! A worker sitting at the soft limit must checkpoint 16 MiB without growing
//! its heap. `StateDigest` is 64 bytes of ticket, not storage; the body is
//! the app's job.
//!
//! * `--supervisor` — samples the child's logical RSS through an APP-owned
//!   sampler and runs [`Supervisor::run_supervised_guard_with_memory`].
//!   After the worker dies it validates the sealed checkpoint through the
//!   crate's stdin handoff (`Supervisor::spawn_with_digest` → `--successor`).
//! * `--worker` (default path) — streaming: `.tmp` → 64 KiB stack buffer →
//!   `sync_all` → `rename` → seal → emit the 64 B ticket.
//! * `--naive` — `state.clone()` before writing. That second 16 MiB copy
//!   takes the worker past `hard_kb`, so the supervisor kills it on the next
//!   10 ms tick, mid-`write_all`, before `rename`: a `.tmp` with no ticket.
//!
//! Run with:
//! ```powershell
//! cargo run --example 16_streaming_checkpoint -- --supervisor
//! cargo run --example 16_streaming_checkpoint -- --supervisor --naive
//! ```

use snubber::{take_stdin_digest, EnterError, MemoryPolicy, SnubberNode, StateDigest, Supervisor};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[path = "_shared/checkpoint.rs"]
mod checkpoint;

use checkpoint::{
    make_state, read_ticket, seal, tmp_path, verify, write_checkpoint, write_ticket, BUF_BYTES,
    FILE_ID, HARD_KB, RSS_AT_SOFT_KB, RSS_NAIVE_KB, RSS_STREAM_KB, SOFT_KB, STATE_BYTES,
};

const DRAIN_MS: u64 = 150;
const NAIVE_TICK_MS: u64 = 5;

fn main() {
    println!("=== Pattern 16: Streaming Checkpoint ===");
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(
        arg_value(&args, "--work-dir").unwrap_or_else(|| "target/checkpoint_work".to_string()),
    );
    let naive = args.iter().any(|a| a == "--naive");

    if args.iter().any(|a| a == "--successor") {
        std::process::exit(successor(&dir));
    }
    if args.iter().any(|a| a == "--worker") {
        worker(&dir, naive);
        return;
    }
    if !args.iter().any(|a| a == "--supervisor") {
        println!("usage: 16_streaming_checkpoint --supervisor [--naive] [--work-dir PATH]");
        return;
    }
    supervisor(&dir, naive);
}

fn supervisor(dir: &Path, naive: bool) {
    let _ = std::fs::create_dir_all(dir);
    // Artifacts from a previous generation must never be mistaken for this
    // one: a stale `state.sncb` would validate against a fresh ticket and a
    // stale `usage.kb` would kill the child before its first sample.
    for name in ["state.sncb", "state.tmp", "state.ticket", "usage.kb"] {
        let _ = std::fs::remove_file(dir.join(name));
    }

    let usage = dir.join("usage.kb");
    let policy = MemoryPolicy::new(SOFT_KB, HARD_KB);
    let sample = move |_pid: u32| {
        std::fs::read_to_string(&usage)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    Supervisor::run_supervised_guard_with_memory("--worker", 0, policy, sample, |token| {
        println!(
            "[APP] rescued armed token task={} opcode={}",
            token.task, token.opcode
        );
    });

    // `run_supervised_guard_with_memory` owns its `PipeDrain`, so the worker
    // also persisted the 64 B ticket next to the sealed file. A hand-rolled
    // supervisor would read `drain.last_state()` instead.
    let ticket = read_ticket(&dir.join("state.ticket")).ok().flatten();
    let successor = match ticket.as_ref() {
        Some(digest) => spawn_successor(dir, digest),
        None => None,
    };
    println!(
        "[VERIFY] mode={} sealed={} tmp={} ticket={} successor={}",
        if naive { "naive" } else { "streaming" },
        dir.join("state.sncb").is_file(),
        dir.join("state.tmp").is_file(),
        ticket.is_some(),
        successor.map_or_else(|| "skipped".to_string(), |code| code.to_string()),
    );
}

fn spawn_successor(dir: &Path, digest: &StateDigest) -> Option<i32> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--successor").arg("--work-dir").arg(dir);
    let mut child = Supervisor::spawn_with_digest(cmd, digest).ok()?;
    child.wait().ok().and_then(|status| status.code())
}

fn worker(dir: &Path, naive: bool) {
    let _ = std::fs::create_dir_all(dir);
    let node = SnubberNode::builder("checkpoint/streaming")
        .memory_limits(SOFT_KB, HARD_KB)
        .build();
    let state = make_state(STATE_BYTES);
    let usage = dir.join("usage.kb");
    let state_path = dir.join("state.sncb");

    // 1. Cross soft: admissions bounce, in-flight guards settle.
    publish(&usage, RSS_AT_SOFT_KB);
    let verdict = node.note_memory_kb(RSS_AT_SOFT_KB);
    match node.enter(1, 1, || {}) {
        Err(EnterError::MemoryPressure) => println!("[WORKER] admission refused at soft"),
        _ => println!("[WORKER] unexpected admission at soft"),
    }
    println!(
        "[WORKER] verdict={} used_kb={RSS_AT_SOFT_KB} — draining state={} B",
        verdict.code(),
        state.len()
    );
    std::thread::sleep(Duration::from_millis(DRAIN_MS));

    if naive {
        naive_checkpoint(&node, &usage, &state_path, &state);
        return;
    }

    // 2. Streaming: the only new memory is one 64 KiB stack buffer.
    let used = node.note_memory_kb(RSS_STREAM_KB);
    publish(&usage, RSS_STREAM_KB);
    let crc = write_checkpoint(&state_path, &state).expect("streaming checkpoint");
    let digest = seal(generation(), 1, state.len() as u64, crc, &FILE_ID);
    // Ticket law: the 64 B ticket exists only after `rename` sealed the file.
    write_ticket(&dir.join("state.ticket"), &digest).expect("persist ticket");
    digest.emit_if_supervised();
    println!(
        "[WORKER] sealed bytes={} crc=0x{crc:08X} verdict={}",
        state.len(),
        used.code()
    );
    std::process::exit(0);
}

/// The failure mode the docs warn about: serialize into RAM first.
fn naive_checkpoint(node: &SnubberNode, usage: &Path, state_path: &Path, state: &[u8]) {
    let copy = state.to_vec(); // +16 MiB over the soft level -> past hard
    let tmp = tmp_path(state_path);
    let mut file = std::fs::File::create(&tmp).expect("create tmp");
    // `.tmp` exists before the fatal sample, so the kill leaves it behind.
    let used = node.note_memory_kb(RSS_NAIVE_KB);
    publish(usage, RSS_NAIVE_KB);
    println!(
        "[WORKER] naive clone +{} KiB verdict={} — writing unsealed .tmp",
        STATE_BYTES / 1024,
        used.code()
    );
    // Slow unsealed write: the supervisor kills mid-`write_all`; `rename`
    // and the seal never run.
    for chunk in copy.chunks(BUF_BYTES) {
        let _ = file.write_all(chunk);
        std::thread::sleep(Duration::from_millis(NAIVE_TICK_MS));
    }
    println!("[WORKER] naive write completed — supervisor missed the crossing");
    std::process::exit(0);
}

fn successor(dir: &Path) -> i32 {
    let Some(digest) = take_stdin_digest() else {
        println!("[SUCCESSOR] dead_ticket reason=\"no 64 B ticket on stdin\"");
        return 2;
    };
    match verify(&digest, &dir.join("state.sncb")) {
        Ok(total) => {
            println!(
                "[SUCCESSOR] restored_bytes={total} file_id=\"{}\" admits again",
                String::from_utf8_lossy(&digest.bytes[40..56])
            );
            0
        }
        Err(e) => {
            println!("[SUCCESSOR] dead_ticket reason=\"{e}\"");
            3
        }
    }
}

fn generation() -> u64 {
    std::env::var("SNUBBER_GEN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

fn publish(path: &Path, used_kb: u64) {
    let _ = std::fs::write(path, used_kb.to_string());
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
