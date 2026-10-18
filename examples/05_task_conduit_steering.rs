//! Pattern 05: Task Conduit Steering Protocol
//!
//! Two-way communication protocol between caller and worker:
//! - Worker (`TaskConduit`) reporting live telemetry state & stage.
//! - Observer (`TaskHandle`) non-blocking state inspection (`inspect`).
//! - Observer issuing runtime directives (`Pause`, `Proceed`, `SkipStage`, `Cancel`).
//! - Shared operating table (`publish_draft` / `take_draft`) between stages.
//!
//! Run with:
//! ```powershell
//! cargo run --example 05_task_conduit_steering
//! ```

use snubber::{Directive, TaskConduit};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerTelemetry {
    pub stage: &'static str,
    pub items_processed: usize,
    pub battery_level: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerDraft {
    pub checksum: u64,
    pub partial_digest: String,
}

fn main() {
    println!("[CONDUIT:INIT] starting bidirectional task steering demo");

    // 1. Establish the communication pair (telemetry state + draft table)
    let initial_telemetry = WorkerTelemetry {
        stage: "Init",
        items_processed: 0,
        battery_level: 100,
    };
    let (conduit, handle) = TaskConduit::<WorkerTelemetry, WorkerDraft>::pair(initial_telemetry);

    // 2. Launch worker thread
    let worker_join = thread::spawn(move || {
        println!("[WORKER:STAGE] stage=Calibration status=started");
        conduit.update_state(|s| {
            s.stage = "Calibration";
            s.battery_level = 98;
        });

        // Stage 1 loop with directive checks
        for i in 1..=5 {
            if let Directive::Cancel(reason) = conduit.check_directive() {
                println!("[WORKER:ABORT] stage=Calibration reason={reason}");
                return Err(reason);
            }
            thread::sleep(Duration::from_millis(20));
            conduit.update_state(|s| {
                s.items_processed = i;
            });
        }

        // Publish draft after Stage 1
        println!("[WORKER:DRAFT] stage=Calibration status=published");
        conduit.publish_draft(WorkerDraft {
            checksum: 0xDEADBEEF,
            partial_digest: "calibrated_matrix_v1".to_string(),
        });

        // Stage 2: Heavy computation, susceptible to Pause or Skip
        println!("[WORKER:STAGE] stage=HeavyCompute status=started");
        conduit.update_state(|s| {
            s.stage = "HeavyCompute";
        });

        for i in 6..=10 {
            match conduit.check_directive() {
                Directive::Proceed => {}
                Directive::Pause => {
                    println!("[WORKER:PAUSE] status=holding_on_condvar");
                    if let Err(reason) = conduit.wait_for_resume() {
                        println!("[WORKER:CANCEL] status=cancelled_while_paused reason={reason}");
                        return Err(reason);
                    }
                    println!("[WORKER:RESUME] status=resumed");
                }
                Directive::SkipStage => {
                    println!("[WORKER:DIRECTIVE] action=skip_stage target=stage_2");
                    break;
                }
                Directive::Cancel(reason) => {
                    println!("[WORKER:CANCEL] reason={reason}");
                    return Err(reason);
                }
                Directive::Preempted(reason) => {
                    println!("[WORKER:PREEMPT] reason={reason}");
                    return Err(reason);
                }
            }
            thread::sleep(Duration::from_millis(25));
            conduit.update_state(|s| {
                s.items_processed = i;
                s.battery_level = s.battery_level.saturating_sub(5);
            });
        }

        println!("[WORKER:STAGE] stage=Complete status=clean");
        conduit.update_state(|s| {
            s.stage = "Complete";
        });

        Ok(())
    });

    // 3. Controller (Observer) observing and steering the worker
    thread::sleep(Duration::from_millis(40));

    // Non-blocking telemetry inspection
    let status = handle.inspect();
    println!(
        "[CONTROLLER:SNAPSHOT] stage={} items={} battery={}%",
        status.stage, status.items_processed, status.battery_level
    );

    // Wait until calibration produces draft
    thread::sleep(Duration::from_millis(80));

    if let Some(draft) = handle.take_draft() {
        println!(
            "[CONTROLLER:DRAFT] checksum=0x{:X} digest={}",
            draft.checksum, draft.partial_digest
        );
    }

    // Command: Pause the worker
    println!("[CONTROLLER:DIRECTIVE] action=pause");
    handle.send_directive(Directive::Pause);

    thread::sleep(Duration::from_millis(50));
    let paused_status = handle.inspect();
    println!(
        "[CONTROLLER:SNAPSHOT] state=paused stage={} items={}",
        paused_status.stage, paused_status.items_processed
    );

    // Command: Resume the worker
    println!("[CONTROLLER:DIRECTIVE] action=resume");
    handle.resume();

    // Command: Tell worker to Skip Stage 2
    thread::sleep(Duration::from_millis(30));
    println!("[CONTROLLER:DIRECTIVE] action=skip_stage");
    handle.send_directive(Directive::SkipStage);

    // Await worker completion
    let res = worker_join.join().expect("Worker thread panicked");
    assert!(res.is_ok(), "Worker should finish cleanly");

    let final_status = handle.inspect();
    println!(
        "[CONTROLLER:FINAL] stage={} items={} battery={}%",
        final_status.stage, final_status.items_processed, final_status.battery_level
    );
    println!("[CONDUIT:SUCCESS] bidirectional steering completed");
}
