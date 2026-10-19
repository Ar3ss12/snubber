//! Pattern 12: ISR bridge — hardware interrupt to guard abort.
//!
//! Simulates the firmware wiring: a timer ISR pushes `OP_PULSE`, an
//! overcurrent ISR latches a trip bit (`trip()`) that bypasses the
//! ring. The main loop drains the latch FIRST (`take_trips()`), then
//! pops the ring and steers the snubber — `pulse.kick()` on pulse,
//! `guard.abort("hardware_isr_trip")` on trip. No mutex, no `unsafe`,
//! ~2 bus cycles per push.
//!
//! Run: `cargo run --example 12_isr_bridge`
//!
//! Firmware note: the `wfe` park behind `IdlePark` is correct only
//! with `SCB.SCR.SEVONPEND = 1` set once at boot (privileged, app
//! side), or with `SEV` at the end of each ISR. Otherwise an ISR
//! between `is_empty()` and `wfe` loses the wakeup.

use snubber::{
    IsrBridge, IsrSignal, PulseWatch, SnubberNode, WorkerPulse, OP_EMERGENCY_TRIP, OP_PAUSE,
    OP_PULSE,
};

static BRIDGE: IsrBridge = IsrBridge::new();

/// Simulated overcurrent ISR: latches the trip bit, never blocks,
/// never touches the ring (survives `Full`).
fn overcurrent_isr(sensor_id: u8) {
    BRIDGE.trip(sensor_id);
}

/// Simulated timer ISR: pushes a heartbeat tick.
fn timer_isr(tick: u16) {
    let _ = BRIDGE.try_push(IsrSignal::new(OP_PULSE, 0, tick));
}

/// Hot-loop poll helper (app side): one `Acquire` directive load next
/// to one `Acquire` bridge probe — ~2 ns total, no locks, no sleep.
#[inline]
fn poll_hardware(conduit_empty: bool) -> bool {
    BRIDGE.has_trips() || (conduit_empty && !BRIDGE.is_empty())
}

fn drain_bridge(pulse: &WorkerPulse) -> bool {
    // Latch first, ring second: trips bypass a full ring.
    let mut tripped = false;
    let trips = BRIDGE.take_trips();
    if trips != 0 {
        println!("[Main] trip word 0x{trips:08X} — aborting");
        tripped = true;
    }
    while let Some(signal) = BRIDGE.try_pop() {
        match signal.opcode() {
            OP_PULSE => pulse.kick(),
            OP_EMERGENCY_TRIP => {
                println!(
                    "[Main] ring trip copy from sensor {} — aborting",
                    signal.reason()
                );
                tripped = true;
            }
            OP_PAUSE => println!("[Main] pause request"),
            _ => {}
        }
    }
    tripped
}

fn main() {
    println!("=== Pattern 12: ISR bridge (interrupt to guard) ===");

    let node = SnubberNode::builder("isr/laser").max_in_flight(1).build();
    let pulse = WorkerPulse::new();
    let mut watch = PulseWatch::new(1000);

    // Cycle 1: timer ticks feed the pulse, guard commits clean.
    println!("\nCycle 1: timer pulse, clean commit");
    timer_isr(1);
    let guard = node
        .enter(1, 1, || println!("[Guard] laser off"))
        .expect("admit");
    assert!(poll_hardware(true), "pulse tick pending");
    assert!(!drain_bridge(&pulse), "no trip latched");
    assert!(watch.observe(pulse.tick(), 100));
    guard.commit();
    println!("[Main] committed: pulse alive, iron energized");

    // Cycle 2: overcurrent ISR trips mid-work, guard aborts.
    println!("\nCycle 2: overcurrent trip, abort");
    let guard = node
        .enter(2, 2, || println!("[Guard] laser off"))
        .expect("admit");
    overcurrent_isr(7);
    assert!(poll_hardware(true), "trip latched");
    assert!(drain_bridge(&pulse), "trip must abort the guard");
    guard.abort("hardware_isr_trip");
    node.note_pulse_stall(watch.idle_ms(5000));
    assert_eq!(node.stats().pulse_stalls, 1);

    // Cycle 3: ring saturated with telemetry (16/16) — the 17th
    // interrupt still passes through the latch, bypassing the queue.
    println!("\nCycle 3: full ring, trip bypasses via latch");
    for i in 0..16u16 {
        timer_isr(i);
    }
    assert!(BRIDGE.try_push(IsrSignal::new(OP_PULSE, 0, 99)).is_err());
    let shed_before = BRIDGE.shed_count();
    assert!(shed_before >= 1, "full ring sheds telemetry");
    overcurrent_isr(4);
    let guard = node
        .enter(3, 3, || println!("[Guard] laser off"))
        .expect("admit");
    assert!(
        drain_bridge(&pulse),
        "latched trip aborts despite full ring"
    );
    guard.abort("hardware_isr_trip");

    println!("\nVerification: ISR trip aborted iron without a single lock.");
}
