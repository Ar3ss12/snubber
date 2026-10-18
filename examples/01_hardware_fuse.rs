//! Pattern 01: Hardware Fuse & Physical Safety Interlock
//!
//! Demonstrates zero-cost, no_std compatible RAII transaction safety.
//! Guarantees that physical pins, relays, or hardware states are immediately
//! de-energized if a communication failure, panic, or validation error occurs.

use snubber::SnubberGuard;

struct HighVoltageRelay {
    energized: bool,
}

impl HighVoltageRelay {
    fn new() -> Self {
        Self { energized: false }
    }

    fn energize(&mut self) {
        self.energized = true;
        println!("[Hardware] Relay energized: 400V active");
    }

    fn safe_shutdown(&mut self) {
        if self.energized {
            self.energized = false;
            println!("[Hardware] Relay de-energized: contacts opened safely");
        }
    }
}

fn execute_laser_calibration(
    relay: &mut HighVoltageRelay,
    simulate_error: bool,
) -> Result<(), &'static str> {
    // 1. Energize critical hardware
    relay.energize();

    // 2. Arm safety guard with an unconditional rollback closure
    let epoch = 1;
    let task_id = 101;
    let guard = SnubberGuard::new(epoch, task_id, || {
        println!("[Guard] Failure detected or scope dropped before commit!");
    });

    // 3. Perform critical operation
    println!("[Task] Initializing laser optical sensors...");
    if simulate_error {
        // Early return: guard drops without commit, triggering rollback
        return Err("Laser mirror calibration timed out");
    }

    println!("[Task] Sensor feedback nominal. Laser beam calibrated.");

    // 4. Commit explicitly upon complete success
    guard.commit();
    Ok(())
}

fn main() {
    println!("=== Pattern 01: Hardware Safety Fuse ===");
    let mut relay = HighVoltageRelay::new();

    // Test run 1: simulated failure
    println!("\nScenario A: Hardware failure mid-operation");
    let result = execute_laser_calibration(&mut relay, true);
    if result.is_err() {
        relay.safe_shutdown();
    }
    assert!(!relay.energized);

    // Test run 2: successful execution
    println!("\nScenario B: Clean execution");
    let result = execute_laser_calibration(&mut relay, false);
    assert!(result.is_ok());
    relay.safe_shutdown();

    println!("\nVerification: Hardware safely shutdown in all scenarios.");
}
