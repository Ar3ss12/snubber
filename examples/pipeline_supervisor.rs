//! Autopilot assembly (all phases): gateway + priority + restart + handoff.
//!
//! Run: `cargo run --example autopilot`
//!
//! One file showing every primitive wired together: doors send intents
//! through the gateway, the host mutates ONE enum via transition,
//! Critical preempts Background, tracker guards the storm, digest
//! survives death. Steel thread, no framework.
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
    let node = SnubberNode::builder("autopilot")
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
    println!("AUTOPILOT: gateway drained, state=Idle");
    let bg = node
        .enter_with_priority(Priority::Background, 100, 100, || {})
        .expect("bg");
    assert!(node.lane_in_flight() == [1, 0, 0]);
    drop(bg);
    println!("AUTOPILOT: priority lanes live");
    let mut tracker = RestartTracker::new(
        RestartPolicy::OnCrash,
        Backoff::exponential(50, 1000),
        RestartWindow::new(3, 10),
    );
    match tracker.note_death(Some(1), 1000) {
        RestartDecision::Restart { delay_ms, .. } => {
            println!("AUTOPILOT: restart delay={delay_ms}ms")
        }
        other => panic!("must restart: {other:?}"),
    }
    let mut bytes = [0u8; 64];
    bytes[0] = 42;
    let digest = StateDigest::new(bytes);
    assert_eq!(digest.bytes[0], 42);
    println!("AUTOPILOT: digest mark=42, all phases wired");
}
