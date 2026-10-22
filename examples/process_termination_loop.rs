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
        .unwrap_or_else(|| "target/termination_report.json".to_string());

    let spin = args.iter().any(|a| a == "--spin");
    let armed_hold = args.iter().any(|a| a == "--armed-hold");

    let pid_file = "target/supervised_worker.pid";
    if let Some(parent) = std::path::Path::new(pid_file).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    println!(
        "[SUPERVISOR:ONLINE] target_kills={target_kills} protocol=framed_pipe_snub_token armed_hold={armed_hold}"
    );

    let mut clock = GenerationClock::new();
    let start_time = Instant::now();
    let current_exe = std::env::current_exe().expect("current exe");

    let mut total_reaped = 0u32;
    let mut tokens_rescued = 0u32;
    let mut clean_settles = 0u32;
    let mut ambush_kills = 0u32;
    let mut escape_kills = 0u32;
    let mut stillborn_kills = 0u32;
    let mut age_min_us = u64::MAX;
    let mut age_max_us = 0u64;
    let mut age_sum_us = 0u64;

    while total_reaped < target_kills {
        let gen = clock.generation();

        // Spawn disposable child worker process
        let mut cmd = Command::new(&current_exe);
        cmd.arg("--worker");
        if spin {
            cmd.arg("--worker-spin");
        }
        if armed_hold {
            cmd.arg("--worker-hold");
        }
        let spawned_at = Instant::now();
        let mut child = cmd
            .env("SNUBBER_SUPERVISED", "1")
            .env("SNUBBER_GEN", gen.to_string())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn worker");

        let pid = child.id();
        if !armed_hold {
            // Publish current worker PID for the external process killer / fault injector
            let _ = std::fs::write(pid_file, pid.to_string());
        }

        let reader = child.stdout.take().expect("piped stdout");
        let drain = PipeDrain::spawn(reader);

        if armed_hold {
            // The worker emits one arm frame and never commits. Publish its PID
            // only after the arm is visible, so every shot is a true ambush
            // instead of a kill during process startup.
            let armed_deadline = Instant::now() + Duration::from_millis(100);
            while drain.counters().arms == 0 && Instant::now() < armed_deadline {
                std::thread::sleep(Duration::from_micros(200));
            }
            let _ = std::fs::write(pid_file, pid.to_string());
        }

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
        let age_us = spawned_at.elapsed().as_micros() as u64;
        age_min_us = age_min_us.min(age_us);
        age_max_us = age_max_us.max(age_us);
        age_sum_us += age_us;
        let counters = drain.counters();
        let report_cycle =
            total_reaped % 100 == 0 || total_reaped <= 10 || total_reaped == target_kills;

        // Path-2 verification: did the worker die leaving an armed hardware token?
        if let Some(token) = drain.take_armed() {
            ambush_kills += 1;
            tokens_rescued += 1;
            if report_cycle {
                println!(
                    "[SUPERVISOR:REAP] cycle={total_reaped}/{target_kills} pid={pid} gen={gen} class=ambush age_us={age_us} frames={} arms={} commits={} task={} op={}",
                    counters.frames, counters.arms, counters.commits, token.task, token.opcode
                );
            }
        } else {
            // No armed token: either the worker committed before death (escape)
            // or it died before its first commit left nothing to rescue.
            let class = if counters.commits > 0 {
                escape_kills += 1;
                "escape"
            } else {
                stillborn_kills += 1;
                "stillborn"
            };
            clean_settles += 1;
            if report_cycle {
                println!(
                    "[SUPERVISOR:REAP] cycle={total_reaped}/{target_kills} pid={pid} gen={gen} class={class} age_us={age_us} frames={} arms={} commits={} status={status:?}",
                    counters.frames, counters.arms, counters.commits
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
        "[SUPERVISOR:SUMMARY] total_reaped={total_reaped} target_kills={target_kills} tokens_rescued={tokens_rescued} clean_settles={clean_settles} ambush={ambush_kills} escape={escape_kills} stillborn={stillborn_kills} armed_hold={armed_hold} final_gen={final_gen} elapsed_ms={}",
        elapsed.as_millis()
    );

    if let Some(parent) = std::path::Path::new(&report_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let age_mean_us = if total_reaped > 0 {
        age_sum_us / u64::from(total_reaped)
    } else {
        0
    };
    let age_min = if total_reaped > 0 { age_min_us } else { 0 };

    let report_json = format!(
        r#"{{
  "target_kills": {},
  "total_reaped": {},
  "tokens_rescued": {},
  "clean_settles": {},
  "ambush_kills": {},
  "escape_kills": {},
  "stillborn_kills": {},
  "armed_hold": {},
  "age_us": {{ "min": {}, "mean": {}, "max": {} }},
  "final_generation": {},
  "deadlock": false,
  "elapsed_ms": {}
}}"#,
        target_kills,
        total_reaped,
        tokens_rescued,
        clean_settles,
        ambush_kills,
        escape_kills,
        stillborn_kills,
        armed_hold,
        age_min,
        age_mean_us,
        age_max_us,
        final_gen,
        elapsed.as_millis()
    );

    let mut f = File::create(&report_path).expect("create report file");
    f.write_all(report_json.as_bytes()).expect("write report");
}

/// Supervised child worker: arms hardware tokens and commits rapidly in a tight loop.
///
/// With `--worker-hold` it arms exactly once and parks, never committing:
/// the process can only end by external termination, leaving the token armed.
fn worker_main() {
    use std::io::Write;
    let spin = std::env::args().any(|a| a == "--worker-spin");
    let hold = std::env::args().any(|a| a == "--worker-hold");
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

        if hold {
            // Stay armed until the killer reaps us: no commit, no self-exit.
            loop {
                std::thread::park();
            }
        }

        // Tiny window where token is actively armed (target for external termination)
        wait_ms(3, spin);

        // 2. Settle clean
        let commit = ProcessToken::commit(task_id);
        if out.write_all(&commit.to_bytes()).is_err() || out.flush().is_err() {
            break;
        }

        // Fast pacing
        wait_ms(1, spin);
    }
}

/// Sleep pacing is subject to the OS timer quantum (~15.6 ms on Windows by default),
/// which stretches the 3+1 ms cycle to ~30 ms. Spin pacing keeps the harness cycle
/// at the real duration and is what `stress_harness -Aggressive` selects.
fn wait_ms(ms: u64, spin: bool) {
    if spin {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            std::hint::spin_loop();
        }
    } else {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
