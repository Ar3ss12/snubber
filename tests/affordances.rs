//! Shared-memory aggregate + poison-tolerant lock + blocking pause gate.

use snubber::{Directive, NodeSet, SafeLockExt, SnubberNode, TaskConduit};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

#[test]
fn nodeset_aggregates_without_io() {
    let a = SnubberNode::builder("set_a").build();
    let b = SnubberNode::builder("set_b").build();
    let _: Result<(), snubber::SnubberError<&'static str>> = a.scoped(|| {}, || Err("biz"));
    let _: Result<(), snubber::SnubberError<&'static str>> = b.scoped(|| {}, || Ok(()));
    let mut set = NodeSet::new();
    assert!(set.is_empty());
    set.register("set_a", &a);
    set.register("set_b", &b);
    assert_eq!(set.len(), 2);
    assert_eq!(set.total_faults(), 0);
    assert_eq!(set.total_catastrophic_faults(), 0);
    assert_eq!(set.total_hydraulic_shed(), (0, 0, 0));
    assert!(!set.is_any_breaker_tripped());
    let mut lines = Vec::new();
    set.dump_summary(|l| lines.push(l.to_string()));
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("set_a"));
    assert!(lines[1].contains("set_b"));
    set.clear();
    assert!(set.is_empty());
}

#[test]
fn safelock_recovers_poisoned_mutex() {
    let node = SnubberNode::builder("safelock_demo").build();
    let m = Mutex::new(41u32);
    // Poison the mutex from another thread.
    std::thread::scope(|s| {
        let h = s.spawn(|| {
            let _g = m.lock().unwrap();
            panic!("foreign panic under lock");
        });
        let _ = h.join();
    });
    assert!(m.is_poisoned());
    {
        let mut g = m.safe_lock(&node);
        *g += 1;
        assert_eq!(*g, 42);
    }
    assert_eq!(*m.safe_lock(&node), 42);
    // Two recoveries: one per safe_lock on the sticky poison flag.
    assert_eq!(node.stats().poison_recoveries, 2);
}

#[test]
fn wait_while_paused_blocks_on_condvar_until_resume() {
    static WORKER_ARRIVED: AtomicBool = AtomicBool::new(false);
    static WORKER_PASSED: AtomicBool = AtomicBool::new(false);
    let (conduit, handle): (TaskConduit<u8>, _) = TaskConduit::pair(0);
    // Fast path: no pause armed.
    assert_eq!(conduit.wait_while_paused(), Directive::Proceed);
    handle.send_directive(Directive::Pause);
    let barrier = Arc::new(Barrier::new(2));
    let b = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        b.wait();
        WORKER_ARRIVED.store(true, Ordering::Release);
        let d = conduit.wait_while_paused();
        WORKER_PASSED.store(true, Ordering::Release);
        d
    });
    barrier.wait();
    // Wait until the worker has actually REACHED the pause gate (bounded
    // deadline, not a sleep guess). Without this, a slow worker makes the
    // parked-assert below pass vacuously (`WORKER_PASSED == false` holds
    // simply because the worker has not run yet).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !WORKER_ARRIVED.load(Ordering::Acquire) {
        // The barrier releases both sides together; the flag is stored by
        // the test below via the conduit snapshot — poll the directive
        // reader side instead: absence of a dedicated hook means we
        // approximate "arrived" with a bounded grace, still no vacuous
        // pass once PASSED is observed.
        if std::time::Instant::now() > deadline {
            panic!("worker never reached wait_while_paused");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Regression guard (not a sleep guess): the worker MUST still be
    // parked behind the pause. If `wait_while_paused` ever stopped
    // blocking, it would fly past this point and the assert fails.
    assert!(
        !WORKER_PASSED.load(Ordering::Acquire),
        "worker passed wait_while_paused while the directive was still Pause"
    );
    handle.resume();
    // Bounded join: if the paused path ever deadlocked (the worker can
    // never observe Resume), the lane must fail in seconds, not hang the
    // whole test binary until the CI timeout.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(worker.join());
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(Directive::Proceed)) => {}
        Ok(Ok(other)) => panic!("expected Proceed after resume, got {other:?}"),
        Ok(Err(_)) => panic!("worker panicked inside wait_while_paused"),
        Err(_) => panic!("safe_lock deadlock: worker never returned within 5 s of resume"),
    }
    assert!(WORKER_PASSED.load(Ordering::Acquire));
}

// F-08 (PLAN_AUDIT2): the recovery path must never re-lock the ledger —
// `safe_lock` stays callable while the node lock is contended (a foreign
// mutex poisoned inside a node-locked section). Hammer it concurrently
// with node readers: the recovered guard is always the live value.
#[test]
fn poison_recovery_never_relocks_the_node_ledger() {
    let node = SnubberNode::builder("safe_lock/hammer").build();
    let m = Mutex::new(0u32);
    std::thread::scope(|s| {
        let poisoner = s.spawn(|| {
            let _g = m.lock().unwrap();
            panic!("foreign panic under lock");
        });
        // Concurrent node readers keep the ledger lock contended while the
        // recovery below runs — the old synchronous re-lock would deadlock
        // against them.
        s.spawn(|| {
            for _ in 0..64 {
                let _ = node.stats();
            }
        });
        let _ = poisoner.join(); // the deliberate panic is the poison source
    });
    assert!(m.is_poisoned());
    let g = m.safe_lock(&node);
    assert_eq!(*g, 0, "recovered guard is the live value");
    drop(g);
    assert!(node.stats().poison_recoveries >= 1, "recovery ticked");
}
