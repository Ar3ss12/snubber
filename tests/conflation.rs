//! Conflation contract: one slot, latest wins, coalescing is counted.
//!
//! A raw stream at 10 000 msg/s and a consumer at 60 FPS is a 166:1
//! impedance mismatch. Conflation turns the backlog into a counter instead
//! of heap: the slot holds 0 or 1 value, every replacement increments
//! `conflated`, and the consumer always sees the newest state.

use snubber::ConflateSender;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Miri runs the same assertions on a small sample.
const N: u64 = if cfg!(miri) { 2_000 } else { 1_000_000 };

#[test]
fn latest_wins_over_a_million_events() {
    let (tx, rx) = ConflateSender::<u64>::pair();

    let coalesced = thread::spawn(move || {
        let mut coalesced = 0u64;
        for i in 0..N {
            if tx.publish(i) {
                coalesced += 1;
            }
        }
        coalesced
    })
    .join()
    .expect("producer thread");

    assert_eq!(
        rx.take_latest(),
        Some(N - 1),
        "consumer must see the newest event, not the first"
    );
    assert_eq!(rx.take_latest(), None, "slot is empty after one take");
    assert_eq!(
        coalesced,
        N - 1,
        "every publish but the first replaced an unsampled value"
    );
    assert_eq!(rx.conflated(), N - 1);
}

#[test]
fn many_producers_share_one_slot() {
    let (tx, rx) = ConflateSender::<u64>::pair();
    const PRODUCERS: u64 = 4;

    let mut workers = Vec::new();
    for p in 0..PRODUCERS {
        let tx = tx.clone();
        workers.push(thread::spawn(move || {
            for i in 0..N / PRODUCERS {
                tx.publish(p * N + i);
            }
        }));
    }
    for w in workers {
        w.join().expect("producer thread");
    }

    let last = rx.take_latest().expect("one value survives");
    assert!(
        last / N < PRODUCERS,
        "surviving value {last} carries no producer tag"
    );
    assert_eq!(
        rx.conflated(),
        N - 1,
        "exactly one publish landed in an empty slot"
    );
}

#[test]
fn payload_drops_confined_to_one_slot() {
    struct Tracked {
        _seq: u64,
        alive: Arc<AtomicUsize>,
    }

    impl Tracked {
        fn new(seq: u64, alive: &Arc<AtomicUsize>, peak: &Arc<AtomicUsize>) -> Self {
            let now = alive.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            Self {
                _seq: seq,
                alive: Arc::clone(alive),
            }
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.alive.fetch_sub(1, Ordering::SeqCst);
        }
    }

    let (tx, rx) = ConflateSender::<Tracked>::pair();
    let alive = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    for i in 0..N / 5 {
        tx.publish(Tracked::new(i, &alive, &peak));
    }

    assert_eq!(
        peak.load(Ordering::SeqCst),
        2,
        "at most the slot value + the value being constructed may coexist"
    );
    assert_eq!(
        alive.load(Ordering::SeqCst),
        1,
        "without a consumer exactly one payload is retained"
    );

    let taken = rx.take_latest().expect("newest payload");
    assert_eq!(alive.load(Ordering::SeqCst), 1);
    drop(taken);
    assert_eq!(
        alive.load(Ordering::SeqCst),
        0,
        "slot replacement dropped every superseded payload"
    );
}

#[test]
fn non_clone_payload_moves_through_the_slot() {
    struct Opaque(u64);

    let (tx, rx) = ConflateSender::<Opaque>::pair();
    assert!(!tx.publish(Opaque(7)), "empty slot is a plain publish");
    assert!(tx.publish(Opaque(8)), "occupied slot coalesces");

    let got = rx.take_latest().expect("latest payload");
    assert_eq!(got.0, 8);
}
