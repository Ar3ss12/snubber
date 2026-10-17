//! Supervisor process termination stress loop: Supervisor + disposable worker.
//! Hardened for continuous multi-thousand external process terminations.

use snubber::{GenerationClock, PipeDrain, ProcessToken, SnubberNode};
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OP_HARDWARE_RELEASE: u32 = 7;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--worker") {
        worker_main();
        return;
    }

    let target_kills: u32 = args
        .iter()
        .position(|a| a == "--target-kills")
        .and_then(|idx| args.get(idx + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    let report_path = args
        .iter()
        .position(|a| a == "--report-path")
        .and_then(|idx| args.get(idx + 1))
        .cloned()
        .unwrap_or_else(|| "target/chaos_report.json".to_string());

    let pid_file = "target/chaos_worker.pid";
    if let Some(parent) = std::path::Path::new(pid_file).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    println!("[SUPERVISOR:ONLINE] target_kills={target_kills} protocol=framed_pipe_snub_token");

    let mut clock = GenerationClock::new();
    let start_time = Instant::now();
    let current_exe = std::env::current_exe().expect("current exe");

    let mut total_reaped = 0u32;
    let mut tokens_rescued = 0u32;
    let mut clean_settles = 0u32;

    while total_reaped < target_kills {
        let gen = clock.generation();

        // Spawn disposable satellite child
        let mut child = Command::new(&current_exe)
            .arg("--worker")
            .env("SNUBBER_SUPERVISED", "1")
            .env("SNUBBER_GEN", gen.to_string())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn worker");

        let pid = child.id();
        // Publish current worker PID for the external process killer / fault injector
        let _ = std::fs::write(pid_file, pid.to_string());

        let reader = child.stdout.take().expect("piped stdout");
        let drain = PipeDrain::spawn(reader);

        // Wait for child death (killed by TerminateProcess)
        let status = child.wait().expect("wait on child");

        // Wait for EOF on pipe drain
        let drain_wait_start = Instant::now();
        let mut eof_ok = false;
        while drain_wait_start.elapsed() < Duration::from_millis(500) {
            if drain.eof() {
                eof_ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        if !eof_ok {
            eprintln!("\n[FATAL DEFECT] Pipe drain deadlocked! EOF not observed within 500ms after child death! Reaped={total_reaped}");
            std::process::exit(101);
        }

        total_reaped += 1;

        // Path-2 verification: did the worker die leaving an armed hardware token?
        if let Some(token) = drain.take_armed() {
            tokens_rescued += 1;
            if total_reaped % 100 == 0 || total_reaped <= 10 || total_reaped == target_kills {
                println!(
                    "[SUPERVISOR:REAP] cycle={total_reaped}/{target_kills} pid={pid} gen={gen} token_rescued=true task={} op={}",
                    token.task, token.opcode
                );
            }
        } else {
            clean_settles += 1;
            if total_reaped % 100 == 0 || total_reaped <= 10 || total_reaped == target_kills {
                println!(
                    "[SUPERVISOR:REAP] cycle={total_reaped}/{target_kills} pid={pid} gen={gen} token_rescued=false status={status:?}"
                );
            }
        }

        // Path-1 verification: bump generation, reset epoch
        let next_gen = clock.restart();
        if next_gen != gen + 1 {
            eprintln!(
                "\n[FATAL DEFECT] Generation clock non-monotonic! gen={gen}, next={next_gen}"
            );
            std::process::exit(102);
        }

        // Verify fresh node rejects stale generational lineage
        let node = SnubberNode::builder("super/supervisor").build();
        if let Err(e) = node
            .enter_with_generation(next_gen, 1, 1, || {})
            .map(|g| g.commit())
        {
            eprintln!("\n[FATAL DEFECT] Failed to admit fresh generation {next_gen}: {e:?}");
            std::process::exit(103);
        }
    }

    let elapsed = start_time.elapsed();
    let final_gen = clock.generation();

    // Clean up PID marker
    let _ = std::fs::remove_file(pid_file);

    println!(
        "[SUPERVISOR:SUMMARY] total_reaped={total_reaped} target_kills={target_kills} tokens_rescued={tokens_rescued} clean_settles={clean_settles} final_gen={final_gen} elapsed_ms={}",
        elapsed.as_millis()
    );

    if let Some(parent) = std::path::Path::new(&report_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let report_json = format!(
        r#"{{
  "target_kills": {},
  "total_reaped": {},
  "tokens_rescued": {},
  "clean_settles": {},
  "final_generation": {},
  "deadlock": false,
  "elapsed_ms": {}
}}"#,
        target_kills,
        total_reaped,
        tokens_rescued,
        clean_settles,
        final_gen,
        elapsed.as_millis()
    );

    let mut f = File::create(&report_path).expect("create report file");
    f.write_all(report_json.as_bytes()).expect("write report");
}

/// Satellite worker: arms hardware tokens and commits rapidly in a tight loop.
fn worker_main() {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut task_id = 1000u64;

    loop {
        task_id = task_id.wrapping_add(1);

        // 1. Arm hardware token
        let token = ProcessToken::new(task_id, OP_HARDWARE_RELEASE, [0xFF; 28]);
        if out.write_all(&token.to_bytes()).is_err() || out.flush().is_err() {
            break;
        }

        // Tiny window where token is actively armed (target for external termination)
        std::thread::sleep(Duration::from_millis(3));

        // 2. Settle clean
        let commit = ProcessToken::commit(task_id);
        if out.write_all(&commit.to_bytes()).is_err() || out.flush().is_err() {
            break;
        }

        // Fast pacing
        std::thread::sleep(Duration::from_millis(1));
    }
}
