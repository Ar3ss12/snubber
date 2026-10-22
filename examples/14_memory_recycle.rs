//! Pattern 14: Memory Recycle (soft drain → StateDigest handoff, hard kill).
//!
//! The crate never measures RSS: the APP feeds one sample into
//! `node.note_memory_kb` and the supervisor feeds the same sample into
//! `MemoryPolicy`. This example plays both roles with a synthetic sample
//! sequence:
//!
//! 1. `48 000 KiB` — `Ok`, admissions flow.
//! 2. `70 000 KiB` — `Soft`: admissions refused (`MemoryPressure`), the
//!    in-flight guard drains, the app checkpoint is packed into a 64 B
//!    `StateDigest` and a successor generation is spawned through stdin.
//! 3. `99 000 KiB` — `Hard`: `MemoryPolicy::should_kill()` — terminate
//!    WITHOUT draining (`DeathCause::MemoryLimit`).
//!
//! In a real deployment `Supervisor::run_supervised_guard_with_memory`
//! hosts the same policy with a sampler (`/proc`, `GetProcessMemoryInfo`,
//! cgroup) instead of the table below.
//!
//! Run with:
//! ```powershell
//! cargo run --example 14_memory_recycle
//! ```

use snubber::{
    take_stdin_digest, DeathCause, EnterError, GenerationClock, MemoryPolicy, MemoryVerdict,
    SnubberNode, StateDigest, Supervisor, STATE_LEN,
};

const CHECKPOINT_MARK: u8 = 42;

fn main() {
    println!("=== Pattern 14: Memory Recycle ===");

    // Successor process: first line on stdin is the 64 B digest (same
    // contract as `07_state_handoff.rs`).
    if std::env::args().any(|a| a == "--successor") {
        let mark = take_stdin_digest().map_or(99, |d| d.bytes[0]);
        println!("[SUCCESSOR] restored checkpoint mark={mark} — admits again");
        std::process::exit(mark as i32);
    }

    // gen1 worker: drain at 64 MiB, die at 96 MiB.
    let node = SnubberNode::builder("memory/recycle")
        .memory_limits(64_000, 96_000)
        .build();
    node.enter(1, 1, || {}).expect("gen1 admits").commit();
    println!("[GEN1] admitted, limits={:?} KiB", node.memory_limits_kb());

    // Escalating external samples (KiB).
    let samples = [48_000u64, 70_000, 99_000];
    let mut policy = MemoryPolicy::new(64_000, 96_000);
    let mut epoch = 2u64;
    for used_kb in samples {
        epoch += 1;
        let node_verdict = node.note_memory_kb(used_kb);
        let sup_verdict = policy.note(used_kb);
        println!(
            "[SAMPLE] used={used_kb} KiB node={} supervisor={} kill={}",
            node_verdict.code(),
            sup_verdict.code(),
            policy.should_kill()
        );

        match sup_verdict {
            MemoryVerdict::Ok => {
                node.enter(epoch, used_kb, || {})
                    .expect("under soft limit")
                    .commit();
            }
            MemoryVerdict::Soft => {
                // Drain: new admissions bounce before the lock, in-flight
                // guards settle, then the process hands off and exits.
                match node.enter(epoch, used_kb, || {}) {
                    Err(EnterError::MemoryPressure) => {
                        println!("[DRAIN] admission refused, retry in 1000 ms");
                    }
                    Err(e) => panic!("wrong refusal kind: {e:?}"),
                    Ok(_) => panic!("soft limit must refuse admissions"),
                }
                handoff_and_restart();
            }
            MemoryVerdict::Hard => {
                let cause = policy.death_cause().expect("hard verdict names the cause");
                println!(
                    "[KILL] cause=\"{}\" peak={} KiB action=terminate_no_drain",
                    cause.reason(),
                    policy.watch().peak_kb()
                );
                assert_eq!(cause, DeathCause::MemoryLimit);
            }
        }
    }

    let stats = node.stats();
    println!(
        "\nVerification: rejections={} streak={} breaker={:?} digest_handoff=ok",
        stats.memory_rejections, stats.window_failures, stats.breaker_state
    );
}

/// Pack the checkpoint, spawn the successor on stdin, bump the lineage.
fn handoff_and_restart() {
    let mut bytes = [0u8; STATE_LEN];
    bytes[0] = CHECKPOINT_MARK;
    let digest = StateDigest::new(bytes);
    println!(
        "[HANDOFF] digest_crc=0x{:08X} bytes={}",
        digest.crc(),
        bytes.len()
    );

    let exe = std::env::current_exe().expect("current exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--successor");
    let mut child = Supervisor::spawn_with_digest(cmd, &digest).expect("spawn successor");
    let status = child.wait().expect("reap successor");
    assert_eq!(
        status.code(),
        Some(i32::from(CHECKPOINT_MARK)),
        "checkpoint survived the process boundary"
    );

    // The successor is a fresh generation; the retired one is stale.
    let mut clock = GenerationClock::new();
    let next = clock.restart();
    println!(
        "[RESTART] lineage_bump_gen={next} epoch={}",
        clock.alloc_epoch()
    );
}
