//! Layer 2: gateway HWM/LWM adaptive throttle (hysteresis, pacing, bypass).
use snubber::{gateway, GatewaySendError};

#[test]
fn hysteresis_latch_holds_until_lwm() {
    // cap 8: HWM = 6, LWM = 4.
    let (tx, rx) = gateway::<u32>(8);
    for i in 0..6u32 {
        tx.send(i).expect("room below HWM");
    }
    assert!(tx.throttled(), "latch set at HWM");
    assert!(matches!(
        tx.send(99),
        Err(GatewaySendError::Throttled { .. })
    ));
    // Drain one: depth 5, still above LWM — latch holds, no chatter.
    assert_eq!(rx.try_recv(), Some(0));
    assert!(tx.throttled(), "latch holds above LWM");
    assert!(matches!(
        tx.send(99),
        Err(GatewaySendError::Throttled { .. })
    ));
    // Drain to LWM: depth 4 — latch releases.
    assert_eq!(rx.try_recv(), Some(1));
    assert!(!tx.throttled(), "latch clears at LWM");
    tx.send(99).expect("released at LWM");
}

#[test]
fn retry_after_is_excess_times_service_ms() {
    // cap 128: HWM = 96, LWM = 64. tau driven to 2 ms via EMA samples.
    let (tx, rx) = gateway::<u32>(128);
    for _ in 0..16 {
        rx.note_service_ms(2);
    }
    for i in 0..100u32 {
        tx.send_critical(i).expect("bypass fills to 100");
    }
    match tx.send(1000) {
        Err(GatewaySendError::Throttled {
            retry_after_ms,
            depth,
        }) => {
            assert_eq!(depth, 100);
            assert_eq!(retry_after_ms, (100 - 64) * 2, "excess * tau");
            let bp = GatewaySendError::Throttled {
                retry_after_ms,
                depth,
            }
            .backpressure()
            .expect("throttle carries readings");
            assert_eq!(bp.retry_after_ms, 72);
            assert_eq!(bp.depth, 100);
        }
        other => panic!("must throttle at depth 100, got {other:?}"),
    }
    let _ = rx.try_recv();
}

#[test]
fn critical_bypasses_throttle_until_hard_full() {
    // cap 8: HWM = 6. Fill to latch via critical (bypass never paces).
    let (tx, rx) = gateway::<u32>(8);
    for i in 0..6u32 {
        tx.send_critical(i).expect("bypass below HWM");
    }
    assert!(tx.throttled());
    assert!(matches!(
        tx.send(99),
        Err(GatewaySendError::Throttled { .. })
    ));
    // Critical path still enqueues until the true capacity.
    tx.send_critical(6).expect("bypass to 7");
    tx.send_critical(7).expect("bypass to hard-full");
    assert_eq!(tx.send_critical(8), Err(GatewaySendError::Full));
    // Drain all 8: latch releases on the way down.
    for _ in 0..8 {
        rx.try_recv().expect("item");
    }
    assert!(!tx.throttled());
}

#[test]
fn small_capacity_disables_early_throttle() {
    // cap 2 < 4: no early pacing, only hard Full.
    let (tx, rx) = gateway::<u32>(2);
    assert_eq!(tx.send(1), Ok(()));
    assert!(!tx.throttled());
    assert_eq!(tx.send(2), Ok(()));
    assert_eq!(tx.send(3), Err(GatewaySendError::Full));
    assert!(!tx.throttled(), "no early throttle below cap 4");

    assert_eq!(rx.recv(), Ok(1));
    assert_eq!(tx.send(3), Ok(()));
    assert_eq!(tx.send(4), Err(GatewaySendError::Full));

    drop(rx);
    assert_eq!(tx.send(5), Err(GatewaySendError::Disconnected));

    let st = tx.stats();
    assert_eq!(st.sent, 3);
    assert_eq!(st.admitted, 1);
    assert_eq!(st.shed, 2);
    assert_eq!(st.throttled, 0);
}
