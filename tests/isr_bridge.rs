//! IsrBridge ring drain liveness (0.6.2 regression).
//!
//! A malformed cell (opcode 0 is not a valid opcode) used to make
//! `try_pop` return `None` after consuming the slot — ending the
//! caller's `while let` drain with valid signals still pending behind
//! it. The pop must skip the cell and keep draining.

use snubber::{IsrBridge, IsrSignal, ISR_RING_CAP, OP_PULSE};

#[test]
fn malformed_cell_does_not_stall_the_drain() {
    let bridge = IsrBridge::new();
    assert!(bridge.try_push(IsrSignal::from_raw(0)).is_ok());
    assert!(bridge.try_push(IsrSignal::new(OP_PULSE, 7, 42)).is_ok());
    assert_eq!(bridge.pending(), 2);

    let s = bridge
        .try_pop()
        .expect("valid signal behind a malformed cell");
    assert_eq!(s.opcode(), OP_PULSE);
    assert_eq!(s.reason(), 7);
    assert_eq!(s.value(), 42);

    assert!(bridge.try_pop().is_none());
    assert!(bridge.is_empty());
}

/// One producer, one consumer, ten thousand signals: the SPSC ring must
/// deliver every signal exactly once, in push order, with nothing shed.
/// 10k would be glacial under Miri, so the volume is `cfg!(miri)`-scaled.
#[test]
fn spsc_ring_preserves_fifo_under_concurrency() {
    use std::sync::Arc;

    let n: u64 = if cfg!(miri) { 200 } else { 10_000 };
    let bridge = Arc::new(IsrBridge::new());

    // Both loops are bounded: a broken ring must FAIL this test, not hang
    // the runner until the CI timeout. 30 s is orders of magnitude above
    // the honest runtime (~ms native, ~tens of s under Miri).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

    let producer_bridge = Arc::clone(&bridge);
    let producer = std::thread::spawn(move || {
        for i in 0..n {
            while producer_bridge
                .try_push(IsrSignal::new(OP_PULSE, 1, i as u16))
                .is_err()
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "producer starved pushing #{i}: ring wedged"
                );
                std::hint::spin_loop();
            }
        }
    });

    let consumer_bridge = Arc::clone(&bridge);
    let consumer = std::thread::spawn(move || {
        let mut expected: u16 = 0;
        let mut seen: u64 = 0;
        while seen < n {
            match consumer_bridge.try_pop() {
                Some(s) => {
                    assert_eq!(s.opcode(), OP_PULSE, "corrupted cell in transit");
                    assert_eq!(s.reason(), 1, "corrupted cell in transit");
                    assert_eq!(s.value(), expected, "ring delivered out of order at {seen}");
                    expected = expected.wrapping_add(1);
                    seen += 1;
                }
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "consumer starved at {seen}/{n}: ring lost or reordered signals"
                    );
                    std::hint::spin_loop();
                }
            }
        }
        seen
    });

    producer.join().expect("producer must not panic");
    let seen = consumer.join().expect("consumer must not panic");
    assert_eq!(seen, n, "every pushed signal must be popped exactly once");
    assert!(bridge.is_empty(), "ring must drain completely");
    // `shed_count` counts REFUSED push attempts (a Full ring keeps the
    // unread signals and the producer retries) — it is a pressure gauge,
    // not a loss gauge. The loss gauges are the FIFO order above and
    // `seen == n`.
    assert!(
        bridge.shed_count() > 0 || seen <= ISR_RING_CAP as u64,
        "a 10k push stream against a 16-slot ring must see Full refusals"
    );
}
