//! Layer 2: Single-entry channel gateway (MPSC)
use snubber::{gateway, GatewaySendError, SnubberNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickerCommand {
    Start,
    Stop,
    Toggle,
    Tick,
}

#[test]
#[cfg_attr(miri, ignore)] // sleep-based throttle retry is incompatible with Miri timing
fn four_senders_one_host_reject_new() {
    let (gw, host) = gateway::<ClickerCommand>(8);
    let node = SnubberNode::builder("gw/host")
        .max_in_flight(1)
        .trip_after_failures(0)
        .build();
    let doors = [gw.clone(), gw.clone(), gw.clone(), gw];
    let cmds = [
        ClickerCommand::Start,
        ClickerCommand::Toggle,
        ClickerCommand::Stop,
        ClickerCommand::Tick,
    ];
    let mut handles = Vec::new();
    for (i, door) in doors.into_iter().enumerate() {
        handles.push(std::thread::spawn(move || {
            let mut ok = 0u64;
            let mut shed = 0u64;
            for n in 0..200u32 {
                let c = if n % 50 == 0 {
                    cmds[i]
                } else {
                    ClickerCommand::Tick
                };
                // Throttle is pacing, not loss: sleep the advised wait and retry.
                // On transient Full under concurrent race, count shed and retry with yield.
                loop {
                    match door.send(c) {
                        Ok(()) => {
                            ok += 1;
                            break;
                        }
                        Err(GatewaySendError::Full) => {
                            shed += 1;
                            std::thread::yield_now();
                        }
                        Err(GatewaySendError::Throttled { retry_after_ms, .. }) => {
                            std::thread::sleep(std::time::Duration::from_millis(
                                retry_after_ms.min(5),
                            ));
                        }
                        Err(GatewaySendError::Disconnected) => return (ok, shed),
                        // `GatewaySendError` is #[non_exhaustive]: a future
                        // refusal mode must be handled, not silently retried.
                        Err(_) => return (ok, shed),
                    }
                }
            }
            (ok, shed)
        }));
    }
    let mut admitted = 0u64;
    let mut joined = false;
    loop {
        match host.try_recv() {
            Some(_) => {
                let _g = node.enter(admitted + 1, admitted + 1, || {}).expect("host");
                admitted += 1;
            }
            None => {
                if joined {
                    break;
                }
                let mut all = true;
                for h in handles.iter() {
                    if !h.is_finished() {
                        all = false;
                        break;
                    }
                }
                if all {
                    joined = true;
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }
    let mut ok_sum = 0u64;
    let mut shed_sum = 0u64;
    for h in handles {
        let (ok, shed) = h.join().expect("door");
        ok_sum += ok;
        shed_sum += shed;
    }
    let mut tail = 0u64;
    while host.try_recv().is_some() {
        tail += 1;
    }
    let st = host.stats();
    println!(
        "\n[GATE]: senders=4 sent={} admitted={} shed={} throttled={} tail={}",
        st.sent, st.admitted, st.shed, st.throttled, tail
    );
    assert_eq!(st.sent, ok_sum);
    assert_eq!(st.shed, shed_sum);
    assert_eq!(st.sent, st.admitted + tail);
    assert!(st.throttled > 0, "throttle must pace before hard-full");
    assert_eq!(ok_sum, 800, "paced senders lose nothing");
    assert_eq!(node.stats().poison_recoveries, 0);
}

#[test]
fn sender_rejects_on_full_and_disconnect() {
    let (tx, rx) = gateway::<u32>(2);
    assert_eq!(tx.send(1), Ok(()));
    assert_eq!(tx.send(2), Ok(()));
    assert_eq!(tx.send(3), Err(GatewaySendError::Full));

    assert_eq!(rx.recv(), Ok(1));
    assert_eq!(tx.send(3), Ok(()));
    assert_eq!(tx.send(4), Err(GatewaySendError::Full));

    drop(rx);
    assert_eq!(tx.send(5), Err(GatewaySendError::Disconnected));

    let st = tx.stats();
    assert_eq!(st.sent, 3);
    assert_eq!(st.admitted, 1);
    assert_eq!(st.shed, 2);
}

#[test]
fn depth_counter_returns_to_zero_after_concurrent_storm() {
    // Regression: `on_take` used a load-then-store that skipped the
    // decrement when a take raced the producer's `fetch_add(1)` (item
    // dequeued before its increment landed). Every such race burned one
    // decrement forever; under load the counter crept up until the
    // throttle latch wedged shut on an EMPTY queue. The signed
    // `fetch_sub(1)` makes every send exactly balance every take.
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
    use std::sync::Arc;

    let (tx, rx) = snubber::gateway::<u64>(16);
    const PRODUCERS: usize = 4;
    const PER_PRODUCER: u64 = 4096;
    let sent = Arc::new(AtomicU64::new(0));

    let mut producers = Vec::new();
    for p in 0..PRODUCERS {
        let tx = tx.clone();
        let sent = Arc::clone(&sent);
        producers.push(std::thread::spawn(move || {
            for i in 0..PER_PRODUCER {
                let item = (p as u64) << 32 | i;
                loop {
                    match tx.send_critical(item) {
                        Ok(()) => {
                            sent.fetch_add(1, SeqCst);
                            break;
                        }
                        Err(snubber::GatewaySendError::Full) => {
                            std::thread::yield_now();
                        }
                        Err(e) => panic!("unexpected gateway error: {e:?}"),
                    }
                }
            }
        }));
    }
    let admitted = Arc::new(AtomicU64::new(0));
    let receiver = {
        let admitted = Arc::clone(&admitted);
        std::thread::spawn(move || {
            let total = PER_PRODUCER * PRODUCERS as u64;
            while admitted.load(SeqCst) < total {
                if rx.recv().is_ok() {
                    admitted.fetch_add(1, SeqCst);
                }
            }
        })
    };

    for p in producers {
        p.join().expect("producer");
    }
    receiver.join().expect("receiver");

    let total = PER_PRODUCER * PRODUCERS as u64;
    assert_eq!(sent.load(SeqCst), total, "every send landed");
    assert_eq!(admitted.load(SeqCst), total, "every take delivered");
    let st = tx.stats();
    assert_eq!((st.sent, st.admitted), (total, total), "books balanced");
    assert_eq!(
        tx.depth(),
        0,
        "depth must be exactly zero after a full drain — no lost decrements"
    );
    assert!(!tx.throttled(), "empty queue must not stay latched");
}
