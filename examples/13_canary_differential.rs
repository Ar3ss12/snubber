//! Pattern 13: canary differential — naked pass-through leaks, guarded rescues.
//!
//! Domain-neutral comparative stand: a `VirtualDriver` (single `AtomicBool`
//! held flag, zero Win32/`SendInput`) is driven twice through the SAME
//! workload. Cycle 1 runs on a `passthrough(true)` node — the dummy guard
//! never runs the rollback, the resource stays dirty (`HELD=true`).
//! Cycle 2 runs on a guarded node — `Drop` releases the resource
//! (`HELD=false`). No real hardware moves; the leak is proven in memory.
//!
//! Run: `cargo run --example 13_canary_differential`

use snubber::SnubberNode;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

/// Domain-neutral virtual driver: press/hold/release bookkeeping only.
struct VirtualDriver {
    held: AtomicBool,
}

impl VirtualDriver {
    fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    fn press(&self) {
        self.held.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }

    fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }
}

fn main() {
    println!("=== Pattern 13: canary differential (naked vs guarded) ===");

    let driver = VirtualDriver::new();

    // Cycle 1: naked — pass-through node. The rollback is an invocation probe:
    // if the dummy guard ever ran it, `rollback_ran` would flip.
    println!("\nCycle 1: naked pass-through, fault leaks");
    let naked = SnubberNode::builder("canary/naked")
        .max_in_flight(4)
        .passthrough(true)
        .build();
    let rollback_ran = Cell::new(false);
    driver.press();
    {
        let _guard = naked
            .enter_auto(|| rollback_ran.set(true))
            .expect("transit never refuses");
        // Fault here: dummy Drop runs nothing, driver stays held.
    }
    assert!(
        !rollback_ran.get(),
        "pass-through must not invoke the rollback"
    );
    assert!(driver.is_held(), "naked must leak the hold");
    assert_eq!(naked.stats().passthrough_admissions, 1);
    assert_eq!(naked.stats().total_started, 0);
    println!("[Main] naked: HELD=true leaked, transit=1, started=0");

    // Cycle 2: guarded — Drop performs the real reset.
    println!("\nCycle 2: guarded, Drop rescues");
    let armed = SnubberNode::builder("canary/guarded")
        .max_in_flight(4)
        .trip_after_failures(0)
        .build();
    driver.press();
    {
        let _guard = armed.enter_auto(|| driver.release()).expect("slot free");
        // Same fault shape: guarded Drop releases the driver.
    }
    assert!(!driver.is_held(), "guarded must release the hold");
    assert_eq!(armed.stats().passthrough_admissions, 0);
    println!("[Main] guarded: HELD=false rescued, transit=0");

    println!("\nVerification: naked leaks, guarded rescues — differential proven.");
}
