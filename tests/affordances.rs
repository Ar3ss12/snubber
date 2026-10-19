//! Shared-memory aggregate + poison-tolerant lock + blocking pause gate.

use snubber::{Directive, NodeSet, SafeLockExt, SnubberNode, TaskConduit};
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
    let (conduit, handle): (TaskConduit<u8>, _) = TaskConduit::pair(0);
    // Fast path: no pause armed.
    assert_eq!(conduit.wait_while_paused(), Directive::Proceed);
    handle.send_directive(Directive::Pause);
    let barrier = Arc::new(Barrier::new(2));
    let b = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        b.wait();
        conduit.wait_while_paused()
    });
    barrier.wait();
    // Give the worker a real chance to park (no sleep-poll on OUR side —
    // the worker itself sleeps in the kernel via Condvar).
    std::thread::sleep(Duration::from_millis(50));
    handle.resume();
    assert_eq!(worker.join().expect("worker joins"), Directive::Proceed);
}
