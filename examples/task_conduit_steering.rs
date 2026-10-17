//! Task Conduit Steering Protocol: Two-way communication between caller and worker.
//!
//! Demonstrates:
//! - Worker (`TaskConduit`) reporting live telemetry state & stage.
//! - Observer (`TaskHandle`) non-blocking state inspection (`inspect`).
//! - Observer issuing runtime directives (`Pause`, `Proceed`, `SkipStage`, `Cancel`).
//! - Shared operating table (`publish_draft` / `take_draft`) between stages.
//!
//! Run with:
//! ```powershell
//! cargo run --example task_conduit_steering
//! ```

use snubber::{Directive, TaskConduit};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
struct MissionTelemetry {
    pub stage: &'static str,
    pub items_processed: usize,
    pub battery_level: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MissionDraft {
    pub checksum: u64,
    pub partial_digest: String,
}

fn main() {
    println!("=== Snubber Cockpit / Intercom Protocol Demo ===");

    // 1. Establish the communication cockpit pair (telemetry state + draft table)
    let initial_telemetry = MissionTelemetry {
        stage: "PreFlight",
        items_processed: 0,
        battery_level: 100,
    };
    let (conduit, handle) = TaskConduit::<MissionTelemetry, MissionDraft>::pair(initial_telemetry);

    // 2. Launch worker thread (Pilot)
    let worker_join = thread::spawn(move || {
        println!("[PILOT] Engine started. Entering stage 1: Calibration.");
        conduit.update_state(|s| {
            s.stage = "Calibration";
            s.battery_level = 98;
        });

        // Stage 1 loop with directive checks
        for i in 1..=5 {
            // Check for pause/cancel from cockpit
            if let Directive::Cancel(reason) = conduit.check_directive() {
                println!("[PILOT] Aborted during calibration: {reason}");
                return Err(reason);
            }
            thread::sleep(Duration::from_millis(20));
            conduit.update_state(|s| {
                s.items_processed = i;
            });
        }

        // Publish draft after Stage 1
        println!("[PILOT] Stage 1 complete. Publishing intermediate draft.");
        conduit.publish_draft(MissionDraft {
            checksum: 0xDEADBEEF,
            partial_digest: "calibrated_matrix_v1".to_string(),
        });

        // Stage 2: Heavy computation, susceptible to Pause or Skip
        println!("[PILOT] Entering stage 2: HeavyCompute.");
        conduit.update_state(|s| {
            s.stage = "HeavyCompute";
        });

        for i in 6..=10 {
            match conduit.check_directive() {
                Directive::Proceed => {}
                Directive::Pause => {
                    println!("[PILOT] Received PAUSE directive! Holding position on condvar...");
                    if let Err(reason) = conduit.wait_for_resume() {
                        println!("[PILOT] Cancelled while paused: {reason}");
                        return Err(reason);
                    }
                    println!("[PILOT] RESUMED! Continuing computation.");
                }
                Directive::SkipStage => {
                    println!("[PILOT] Received SKIP STAGE directive! Fast-forwarding stage 2.");
                    break;
                }
                Directive::Cancel(reason) => {
                    println!("[PILOT] Mission cancelled: {reason}!");
                    return Err(reason);
                }
                Directive::Preempted(reason) => {
                    println!("[PILOT] Yielding slot to critical admission: {reason}!");
                    return Err(reason);
                }
            }
            thread::sleep(Duration::from_millis(25));
            conduit.update_state(|s| {
                s.items_processed = i;
                s.battery_level = s.battery_level.saturating_sub(5);
            });
        }

        println!("[PILOT] Flight complete. Landing cleanly.");
        conduit.update_state(|s| {
            s.stage = "Landed";
        });

        Ok(())
    });

    // 3. Cockpit Controller (Observer) observing and steering the pilot
    thread::sleep(Duration::from_millis(40));

    // Non-blocking telemetry inspection
    let status = handle.inspect();
    println!(
        "[COCKPIT] Telemetry snapshot: stage={}, items={}, battery={}%",
        status.stage, status.items_processed, status.battery_level
    );

    // Wait until calibration produces draft
    thread::sleep(Duration::from_millis(80));

    if let Some(draft) = handle.take_draft() {
        println!(
            "[COCKPIT] Inspected draft: checksum=0x{:X}, digest={}",
            draft.checksum, draft.partial_digest
        );
    }

    // Command: Pause the pilot mid-flight
    println!("[COCKPIT] Ordering PAUSE to inspect flight path...");
    handle.send_directive(Directive::Pause);

    thread::sleep(Duration::from_millis(50));
    let paused_status = handle.inspect();
    println!(
        "[COCKPIT] Status while paused: stage={}, items={}",
        paused_status.stage, paused_status.items_processed
    );

    // Command: Resume the pilot
    println!("[COCKPIT] Ordering RESUME...");
    handle.resume();

    // Command: Tell pilot to Skip Stage 2 and land
    thread::sleep(Duration::from_millis(30));
    println!("[COCKPIT] Weather clearing, ordering SKIP STAGE to expedite landing...");
    handle.send_directive(Directive::SkipStage);

    // Await pilot completion
    let res = worker_join.join().expect("Worker thread panicked");
    assert!(res.is_ok(), "Worker should finish cleanly");

    let final_status = handle.inspect();
    println!(
        "[COCKPIT] Final pilot status: stage={}, items={}, battery={}%",
        final_status.stage, final_status.items_processed, final_status.battery_level
    );
    println!("=== Intercom Demo completed successfully! ===");
}
