//! API extensibility: capture-capable hooks, dynamic node names, mapped
//! invariant gates, explicit-argv supervision and published format ceilings.

use snubber::{
    snub_check, snub_check_map, Backpressure, BackpressurePolicy, FaultKind, NodeName, PoolConfig,
    SnubberNode, Supervisor, DEFAULT_EOF_GRACE_MS, ISR_RING_CAP, PROFILE_STAGES, SNAPSHOT_LEN,
    TOKEN_BODY_LEN, TOKEN_PAYLOAD_LEN,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

fn adaptive(max_wait_ms: u64) -> BackpressurePolicy {
    BackpressurePolicy::AdaptiveBlock {
        max_wait_ms,
        shed_deeper_than: None,
    }
}

#[test]
fn friction_closure_captures_live_state() {
    let scale = Arc::new(AtomicU64::new(100));
    let seen = Arc::clone(&scale);
    let node = SnubberNode::builder("friction/capture")
        .max_in_flight(4)
        .backpressure(adaptive(10_000))
        .friction_fn(move |in_flight, max| {
            seen.load(Ordering::Relaxed) * u64::from(in_flight) + u64::from(max)
        })
        .build();

    assert_eq!(node.friction_wait_ms(), 4, "0 in flight, max 4");
    let g = node.enter(1, 1, || {}).expect("admit");
    assert_eq!(node.friction_wait_ms(), 104, "one in flight at scale 100");

    scale.store(7, Ordering::Relaxed);
    assert_eq!(node.friction_wait_ms(), 11, "handler sees the new scale");
    g.commit();
}

#[test]
fn legacy_fn_pointer_and_wrapper_agree() {
    fn fixed(in_flight: u32, _max: u32) -> u64 {
        u64::from(in_flight) + 1
    }

    let plain = SnubberNode::builder("friction/fn")
        .max_in_flight(2)
        .backpressure(adaptive(50))
        .friction_fn(fixed)
        .build();
    let wrapped = SnubberNode::builder("friction/ptr")
        .max_in_flight(2)
        .backpressure(adaptive(50))
        .friction_fn_ptr(fixed)
        .build();

    assert_eq!(plain.friction_wait_ms(), 1);
    assert_eq!(wrapped.friction_wait_ms(), 1);
}

#[test]
fn pressure_closure_runs_outside_the_lock() {
    let seen: Arc<Mutex<Vec<Backpressure>>> = Arc::new(Mutex::new(Vec::new()));
    let cell: Arc<OnceLock<SnubberNode>> = Arc::new(OnceLock::new());

    let sink = Arc::clone(&seen);
    let node_cell = Arc::clone(&cell);
    let node = SnubberNode::builder("pressure/capture")
        .max_in_flight(1)
        .pressure_fn(move |bp| {
            sink.lock().expect("handler lock").push(bp);
            // Re-entering the node would deadlock if the handler ran under
            // the node mutex — the whole point of the outside-lock law.
            let _ = node_cell.get().expect("node published").stats();
        })
        .build();
    assert!(cell.set(node.clone()).is_ok(), "first publish");

    let (tx, rx) = mpsc::channel();
    let worker = node.clone();
    std::thread::spawn(move || {
        let g = worker.enter(1, 1, || {}).expect("first admit");
        let second = worker.enter(2, 2, || {});
        assert!(second.is_err(), "second admission must be refused");
        g.commit();
        tx.send(()).expect("signal");
    });

    rx.recv_timeout(Duration::from_secs(5))
        .expect("pressure handler must not run under the node lock");

    let seen = seen.lock().expect("handler lock");
    assert_eq!(seen.len(), 1, "one saturation, one reaction");
    assert_eq!(seen[0].kind, FaultKind::BulkheadFull);
    assert_eq!(seen[0].depth, 255, "1 of 1 in flight is full depth");
}

#[test]
fn dynamic_node_name_reaches_records() {
    let node = SnubberNode::builder(format!("shard-{}", 7))
        .max_in_flight(1)
        .build();
    assert_eq!(node.name(), "shard-7");
    assert!(!node.name().is_static());

    let _g = node.enter(1, 1, || {}).expect("admit");
    node.report(FaultKind::Panic, 2, 2);
    let fault = node.stats().last_fault.expect("panic recorded");
    assert_eq!(fault.node, "shard-7");
    assert!(!fault.node.is_static());

    let static_node = SnubberNode::builder("static/node").build();
    assert_eq!(static_node.name(), "static/node");
    assert!(static_node.name().is_static());
    assert_eq!(static_node.name().as_static(), Some("static/node"));
}

#[test]
fn node_name_static_and_owned_compare_by_label() {
    let a: NodeName = "worker".into();
    let b = NodeName::owned(String::from("worker"));
    assert_eq!(a, b, "identity is the label, not the storage arm");
    assert_eq!(a, "worker");
    assert_eq!("worker", b);
    assert_eq!(format!("{a}"), "worker");
    assert_eq!(format!("{b:?}"), "\"worker\"");
    assert_eq!(NodeName::default(), "");
}

#[derive(Debug, PartialEq)]
enum GateError {
    Amount,
}

impl From<&'static str> for GateError {
    fn from(_: &'static str) -> Self {
        Self::Amount
    }
}

fn plain(amount: i64) -> Result<(), &'static str> {
    snub_check!(amount != 0, "amount is zero");
    Ok(())
}

fn checked(amount: i64) -> Result<(), GateError> {
    snub_check_map!(amount > 0, "amount must be positive");
    Ok(())
}

#[test]
fn invariant_macros_keep_their_contracts() {
    assert!(plain(5).is_ok());
    assert_eq!(plain(0), Err("amount is zero"));
    assert!(checked(5).is_ok());
    assert_eq!(checked(-1), Err(GateError::Amount));
}

#[test]
fn explicit_argv_supervisor_returns_early() {
    let no_flags = vec![String::from("app")];
    assert!(!Supervisor::run_supervised_guard_argv(
        "--worker",
        &no_flags,
        |_| panic!("must not supervise")
    ));

    let worker = vec![String::from("app"), String::from("--worker")];
    assert!(!Supervisor::run_supervised_guard_with_lease_argv(
        "--worker",
        &worker,
        1_000,
        |_| panic!("must not supervise")
    ));
    assert!(!Supervisor::run_supervised_guard_with_memory_argv(
        "--worker",
        &worker,
        1_000,
        snubber::MemoryPolicy::new(0, 0),
        |_pid| Some(0),
        |_| panic!("must not supervise")
    ));

    let empty: Vec<String> = Vec::new();
    assert!(!Supervisor::run_supervised_guard_argv(
        "--worker",
        &empty,
        |_| panic!("must not supervise")
    ));
}

#[test]
fn format_ceilings_are_published_contracts() {
    assert_eq!(snubber::Priority::COUNT, 3, "three priority lanes");
    assert_eq!(snubber::Priority::ALL.len(), 3);
    assert_eq!(PROFILE_STAGES, 4, "four inline profile stages");
    assert_eq!(ISR_RING_CAP, 16, "sixteen ISR ring slots");
    assert_eq!(SNAPSHOT_LEN, 512, "512 B snapshot frame");
    assert_eq!(TOKEN_PAYLOAD_LEN, 28, "28 B token payload");
    assert_eq!(TOKEN_BODY_LEN, 40, "40 B token body");
    assert_eq!(DEFAULT_EOF_GRACE_MS, 250, "250 ms EOF grace");
    assert_eq!(PoolConfig::new(1).eof_grace_ms, DEFAULT_EOF_GRACE_MS);
}
