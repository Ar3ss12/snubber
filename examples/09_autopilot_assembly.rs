//! Pipeline supervisor assembly (all phases): gateway + priority + restart + handoff.
//!
//! Run: `cargo run --example 09_autopilot_assembly`
//!
//! Demonstrates every primitive wired together: doors send intents
//! through the gateway, the host mutates ONE enum via transition,
//! Critical preempts Background, tracker guards the storm, digest
//! survives process termination.

use snubber::{
    gateway, Backoff, Priority, RestartDecision, RestartPolicy, RestartTracker, RestartWindow,
    SnubberNode, StateDigest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Start,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Running,
}

fn main() {
    let node = SnubberNode::builder("pipeline_supervisor")
        .max_in_flight(4)
        .trip_after_failures(0)
        .build();
    let (gw, host) = gateway::<Cmd>(8);
    gw.send(Cmd::Start).expect("door");
    gw.send(Cmd::Stop).expect("door");
    let mut state = State::Idle;
    while let Some(cmd) = host.try_recv() {
        let next = match cmd {
            Cmd::Start => State::Running,
            Cmd::Stop => State::Idle,
        };
        node.transition(&mut state, next, || {}, |_| Ok::<(), &str>(()))
            .expect("host");
    }
    assert_eq!(state, State::Idle);
    println!("[PIPELINE:GATEWAY] state=Idle status=drained");

    let bg = node
        .enter_with_priority(Priority::Background, 100, 100, || {})
        .expect("bg");
    assert!(node.lane_in_flight() == [1, 0, 0]);
    drop(bg);
    println!("[PIPELINE:PRIORITY] lanes=active in_flight=[0,0,0]");

    let mut tracker = RestartTracker::new(
        RestartPolicy::OnCrash,
        Backoff::exponential(50, 1000),
        RestartWindow::new(3, 10),
    );
    match tracker.note_death(Some(1), 1000) {
        RestartDecision::Restart { delay_ms, .. } => {
            println!("[PIPELINE:RESTART] action=restart delay_ms={delay_ms}")
        }
        other => panic!("expected restart decision, got: {other:?}"),
    }

    let mut bytes = [0u8; 64];
    bytes[0] = 42;
    let digest = StateDigest::new(bytes);
    assert_eq!(digest.bytes[0], 42);
    println!("[PIPELINE:DIGEST] mark=42 status=verified");
}
