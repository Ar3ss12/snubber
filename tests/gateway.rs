//! Phase 4 - Single Entrance Gateway: many senders, ONE receiver.
use snubber::{gateway, GatewaySendError, SnubberNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickerCommand {
    Start,
    Stop,
    Toggle,
    Tick,
}

#[test]
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
                match door.send(c) {
                    Ok(()) => ok += 1,
                    Err(GatewaySendError::Full) => shed += 1,
                    Err(GatewaySendError::Disconnected) => break,
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
        "\n[GATE]: senders=4 sent={} admitted={} shed={} tail={}",
        st.sent, st.admitted, st.shed, tail
    );
    assert_eq!(st.sent, ok_sum);
    assert_eq!(st.shed, shed_sum);
    assert_eq!(st.sent, st.admitted + tail);
    assert!(st.shed > 0);
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
