//! Pattern 01: Hardware Fuse & Physical Safety Interlock
//!
//! Demonstrates zero-cost, no_std compatible RAII transaction safety.
//! Guarantees that physical pins, relays, or actuators are immediately
//! de-energized via Drop unwinding if an early return (?), error, or panic occurs.

use snubber::SnubberGuard;
use std::sync::atomic::{AtomicBool, Ordering};

struct HighVoltageRelay;

impl HighVoltageRelay {
    fn energize(active: &AtomicBool) {
        active.store(true, Ordering::SeqCst);
        println!("[Hardware] Relay energized: 400V active");
    }

    fn safe_shutdown(active: &AtomicBool) {
        if active.swap(false, Ordering::SeqCst) {
            println!("[Hardware] Emergency shutdown: contacts opened safely via RAII Drop");
        }
    }
}

fn execute_laser_calibration(
    relay_active: &'static AtomicBool,
    simulate_error: bool,
) -> Result<(), &'static str> {
    // 1. Energize critical hardware
    HighVoltageRelay::energize(relay_active);

    // 2. Arm safety guard with an unconditional rollback closure.
    // If the function returns early via `?`, panics, or aborts,
    // the guard's Drop destructor runs the rollback closure automatically.
    let epoch = 1;
    let task_id = 101;
    let guard = SnubberGuard::new(epoch, task_id, move || {
        println!("[Guard] Failure detected! Automatic RAII rollback triggered.");
        HighVoltageRelay::safe_shutdown(relay_active);
    });

    // 3. Perform critical operation
    println!("[Task] Initializing laser optical sensors...");
    if simulate_error {
        // Early return: guard drops here without commit, triggering rollback automatically
        return Err("Laser mirror calibration timed out");
    }

    println!("[Task] Sensor feedback nominal. Laser beam calibrated.");

    // 4. Commit explicitly upon complete success
    guard.commit();
    Ok(())
}

static RELAY_ACTIVE: AtomicBool = AtomicBool::new(false);

fn main() {
    println!("=== Pattern 01: Hardware Safety Fuse ===");

    // Scenario A: Hardware failure mid-operation
    println!("\nScenario A: Hardware failure mid-operation (Early Return)");
    let result = execute_laser_calibration(&RELAY_ACTIVE, true);
    assert!(result.is_err());
    // Zero manual cleanup in caller! RAII destructor already de-energized relay:
    assert!(
        !RELAY_ACTIVE.load(Ordering::SeqCst),
        "Relay MUST be de-energized automatically by guard Drop"
    );

    // Scenario B: Clean execution
    println!("\nScenario B: Clean execution (Committed)");
    let result = execute_laser_calibration(&RELAY_ACTIVE, false);
    assert!(result.is_ok());
    // In clean execution, commit was called, so relay remains under caller control:
    assert!(
        RELAY_ACTIVE.load(Ordering::SeqCst),
        "Relay stayed active because operation committed successfully"
    );

    // Caller cleans up only after completing work:
    HighVoltageRelay::safe_shutdown(&RELAY_ACTIVE);
    assert!(!RELAY_ACTIVE.load(Ordering::SeqCst));

    println!(
        "\nVerification: Hardware safely protected by RAII Drop without manual error handling."
    );
}
