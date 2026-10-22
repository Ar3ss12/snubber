//! Pattern 19: Worker Pool (N independent lineages, one pipe per worker).
//!
//! The single-lineage [`Supervisor`](snubber::Supervisor) keeps ONE armed
//! slot and ONE scratch slot inside its `PipeDrain`; several workers behind
//! it would merge rescue accounting. The pool keeps every drain private to
//! its lane and scales the count instead:
//!
//! * `--pool-worker --lane I --local K [--mode armed|clean|silent|orphan]`
//!   — child: `armed` emits one lane-namespaced arm token and waits to be
//!   killed; `clean` arms then commits and exits; `silent` emits nothing;
//!   `orphan` spawns a `--pool-holder` grandchild that inherits the pipe,
//!   then arms and waits.
//! * default (supervisor) — spawns `--lanes N` children, waits until each
//!   one PROVES its mode (arm parsed, commit parsed, or a short startup
//!   pause), kills it from outside (`child.kill()`, app-owned), reaps
//!   through `PoolLane::note_death` and writes the aggregated JSON.
//! * `--mixed` — cycle the four modes by `local % 4`, so one storm proves
//!   all four buckets (ambush / escape / stillborn / deadlocked) at once.
//!
//! Run with:
//! ```powershell
//! cargo run --example 19_worker_pool -- --lanes 4 --kills 5
//! cargo run --example 19_worker_pool -- --lanes 4 --kills 8 --mixed
//! ```
//!
//! Expected evidence per lane: `A/E/S/D=K/0/0/0`, `gen=K+1`, and the
//! aggregate line `conserved=true`. Under `--mixed` the same run reports
//! an equal four-way split and `deadlock=true` — deadlocks are the honest
//! bucket, not a failure.

use snubber::pool::{namespace_task, PoolConfig, PoolLane, ReapClass, WorkerPool};
use snubber::{Backoff, PipeDrain, ProcessToken, RestartDecision, RestartPolicy, RestartWindow};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Emergency opcode armed by the demo worker (any non-commit opcode arms).
const OP_ARMED: u32 = 9;
/// Mode names in `local % 4` order for `--mixed`.
const MODES: [&str; 4] = ["armed", "clean", "silent", "orphan"];

fn main() {
    println!("=== Pattern 19: Worker Pool ===");
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--pool-worker") {
        worker(&args);
        return;
    }
    if args.iter().any(|a| a == "--pool-holder") {
        holder(&args);
        return;
    }

    let lanes = arg_num(&args, "--lanes").unwrap_or(4) as usize;
    let kills = arg_num(&args, "--kills").unwrap_or(5);
    let mixed = args.iter().any(|a| a == "--mixed");
    let grace_ms = arg_num(&args, "--grace-ms").unwrap_or(if mixed { 150 } else { 1_000 });
    let report_path =
        arg_value(&args, "--report").unwrap_or_else(|| "target/pool_report.json".to_string());
    supervisor(lanes, kills, &report_path, mixed, grace_ms);
}

/// Child role: one frame in its mode, then silence until the supervisor
/// kills it (or a clean exit for `clean`).
fn worker(args: &[String]) {
    let lane = arg_num(args, "--lane").unwrap_or(0) as u16;
    let local = arg_num(args, "--local").unwrap_or(0);
    let mode = arg_value(args, "--mode").unwrap_or_else(|| "armed".to_string());
    let token = ProcessToken::simple(namespace_task(lane, local), OP_ARMED, lane as u8);
    let mut stdout = std::io::stdout();

    match mode.as_str() {
        "clean" => {
            let _ = stdout.write_all(&token.to_bytes());
            let _ = stdout.write_all(&ProcessToken::commit(token.task).to_bytes());
            let _ = stdout.flush();
            std::process::exit(0);
        }
        "silent" => {
            std::thread::sleep(Duration::from_millis(40));
            std::process::exit(4);
        }
        "orphan" => {
            // Spawn the holder BEFORE arming: once the supervisor sees the
            // arm frame, the pipe is guaranteed to outlive this child.
            let exe = std::env::current_exe().expect("current_exe");
            let _ = std::process::Command::new(exe)
                .args(["--pool-holder", "--hold-ms", "1500"])
                .stdout(std::process::Stdio::inherit())
                .spawn();
            let _ = stdout.write_all(&token.to_bytes());
            let _ = stdout.flush();
            sleep_until_killed();
        }
        _ => {
            let _ = stdout.write_all(&token.to_bytes());
            let _ = stdout.flush();
            sleep_until_killed();
        }
    }
}

/// Orphan pipe holder: inherits the worker's stdout and outlives it.
fn holder(args: &[String]) {
    let hold_ms = arg_num(args, "--hold-ms").unwrap_or(1_500);
    std::thread::sleep(Duration::from_millis(hold_ms));
    std::process::exit(0);
}

fn sleep_until_killed() -> ! {
    // Cap the sleep so a failed harness cannot leak this process forever.
    for _ in 0..600 {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::process::exit(3);
}

fn supervisor(lanes: usize, kills: u64, report_path: &str, mixed: bool, grace_ms: u64) {
    let config = PoolConfig::new(lanes)
        .with_restart(
            RestartPolicy::Always,
            Backoff::none(),
            RestartWindow::unlimited(),
        )
        .with_eof_grace(grace_ms);
    let pool = WorkerPool::new(config).expect("pool config");
    println!(
        "[POOL] lanes={lanes} kills_per_lane={kills} policy=Always window=unlimited mixed={mixed} grace_ms={grace_ms}"
    );

    let mut handles = Vec::new();
    for index in 0..lanes {
        let lane = Arc::clone(pool.lane(index).expect("lane index"));
        handles.push(std::thread::spawn(move || run_lane(lane, kills, mixed)));
    }
    for handle in handles {
        handle.join().expect("lane thread");
    }

    let report = pool.report();
    println!("\n{report}");

    let generations = report
        .final_generations()
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(
        "{{\"lanes\":{},\"reaped\":{},\"rescued\":{},\"ambush\":{},\"escape\":{},\"stillborn\":{},\"deadlocks\":{},\"deadlock\":{},\"conserved\":{},\"final_generation\":[{}]}}",
        report.totals.lanes,
        report.totals.reaps,
        report.totals.rescued,
        report.totals.ambush,
        report.totals.escape,
        report.totals.stillborn,
        report.totals.deadlocks,
        report.deadlocked(),
        report.conserved(),
        generations
    );
    if let Some(parent) = std::path::Path::new(report_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(report_path, &json);
    println!("[POOL] report written to {report_path}: {json}");

    let expected = (lanes as u64).saturating_mul(kills);
    let ok = if mixed {
        let per_lane = |slot: u64| (kills + (3 - slot)) / 4;
        let want_ambush = lanes as u64 * per_lane(0);
        let want_escape = lanes as u64 * per_lane(1);
        let want_stillborn = lanes as u64 * per_lane(2);
        let want_deadlocked = lanes as u64 * per_lane(3);
        let ok = report.conserved()
            && report.totals.reaps == expected
            && report.totals.ambush == want_ambush
            && report.totals.escape == want_escape
            && report.totals.stillborn == want_stillborn
            && report.totals.deadlocks == want_deadlocked
            && report.totals.rescued == want_ambush;
        println!(
            "[POOL] mixed expected_reaps={expected} want_A/E/S/D={want_ambush}/{want_escape}/{want_stillborn}/{want_deadlocked} got_A/E/S/D={}/{}/{}/{} conserved={}",
            report.totals.ambush,
            report.totals.escape,
            report.totals.stillborn,
            report.totals.deadlocks,
            report.conserved()
        );
        ok
    } else {
        report.healthy() && report.totals.reaps == expected && report.totals.ambush == expected
    };
    println!(
        "[POOL] expected_reaps={expected} actual_reaps={} ambush={} healthy={}",
        report.totals.reaps,
        report.totals.ambush,
        report.healthy()
    );
    std::process::exit(if ok { 0 } else { 1 });
}

/// One lane thread: spawn → attach → wait for the mode proof → kill → reap.
fn run_lane(lane: Arc<PoolLane>, kills: u64, mixed: bool) {
    let index = lane.index();
    let boot = Instant::now();
    let now_ms = || boot.elapsed().as_millis() as u64;

    for local in 0..kills {
        let mode = if mixed {
            MODES[(local % 4) as usize]
        } else {
            "armed"
        };
        let generation = lane.generation();
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                eprintln!("[POOL:LANE {index}] action=current_exe error=\"{error}\"");
                return;
            }
        };
        let mut command = std::process::Command::new(exe);
        command.args([
            "--pool-worker",
            "--lane",
            &index.to_string(),
            "--local",
            &local.to_string(),
            "--mode",
            mode,
        ]);
        command.env("SNUBBER_SUPERVISED", "1");
        command.env("SNUBBER_GEN", generation.to_string());
        command.stdout(std::process::Stdio::piped());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!("[POOL:LANE {index}] action=spawn error=\"{error}\"");
                return;
            }
        };
        let pid = child.id();
        let Some(stdout) = child.stdout.take() else {
            eprintln!("[POOL:LANE {index}] action=stdout error=\"no pipe\"");
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        if let Err(error) = lane.attach(pid, PipeDrain::spawn(stdout)) {
            eprintln!("[POOL:LANE {index}] action=attach error=\"{error}\"");
            let _ = child.kill();
            let _ = child.wait();
            return;
        }

        // Mode proof: kill only after the decisive frame was parsed (or a
        // startup pause for a silent child), so a reaped worker is never a
        // process still in startup.
        match mode {
            "clean" => {
                let deadline = Instant::now() + Duration::from_secs(10);
                while lane.counters().commits == 0 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            "silent" => std::thread::sleep(Duration::from_millis(60)),
            _ => {
                let deadline = Instant::now() + Duration::from_secs(10);
                while lane.counters().arms == 0 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let _ = child.kill();
        let exit_code = child.wait().ok().and_then(|status| status.code());

        let reap = match lane.note_death(exit_code, now_ms()) {
            Ok(reap) => reap,
            Err(error) => {
                eprintln!("[POOL:LANE {index}] action=note_death error=\"{error}\"");
                return;
            }
        };
        let expected_class = match mode {
            "clean" => ReapClass::Escape,
            "silent" => ReapClass::Stillborn,
            "orphan" => ReapClass::Deadlocked,
            _ => ReapClass::Ambush,
        };
        if reap.class != expected_class {
            eprintln!(
                "[POOL:LANE {index}] local={local} mode={mode} expected {expected_class:?} got {:?}",
                reap.class
            );
        }
        println!(
            "[POOL:LANE {index}] local={local} mode={mode} gen={generation} pid={pid} class={:?} rescued={:?} next_gen={} decision={:?}",
            reap.class,
            reap.token.map(|token| token.task),
            reap.generation,
            reap.decision
        );

        match reap.decision {
            RestartDecision::Restart { delay_ms, .. } => {
                if delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(delay_ms));
                }
            }
            RestartDecision::Stop => {
                println!("[POOL:LANE {index}] decision=Stop after local={local}");
                return;
            }
            RestartDecision::GiveUp { deaths_in_window } => {
                eprintln!(
                    "[POOL:LANE {index}] decision=GiveUp deaths_in_window={deaths_in_window}"
                );
                return;
            }
        }
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn arg_num(args: &[String], flag: &str) -> Option<u64> {
    arg_value(args, flag).and_then(|value| value.parse().ok())
}
