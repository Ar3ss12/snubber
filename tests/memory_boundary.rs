//! Memory boundary: the node lock guards node books ONLY (F13).
//!
//! A rollback runs OUTSIDE the lock (Floor-1 owns none at all), so foreign
//! shared state needs its OWN Release→Acquire edge. `Relaxed`×2 may read
//! rot — this test proves the pair holds under hammering. Proves the
//! contract, not the hardware (full proof = loom, see FUTURE_TRAPS Trap 3).

use snubber::SnubberGuard;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn release_acquire_pair_carries_rollback_state() {
    // Foreign handoff: worker publishes DATA then FLAG; observer spins on
    // FLAG then reads DATA. Release→Acquire = observer always sees DATA.
    // (Relaxed×2 is NOT tested for rot here — rot is timing-luck; the pair
    // is tested for holding, which is deterministic.)
    static DATA: AtomicU64 = AtomicU64::new(0);
    static FLAG: AtomicBool = AtomicBool::new(false);
    DATA.store(0, Ordering::Relaxed);
    FLAG.store(false, Ordering::Relaxed);
    let done = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&done);
    let h = std::thread::spawn(move || {
        // Guard rollback publishes via Release (contract 4th MUST).
        let _g = SnubberGuard::new(1, 1, || {
            DATA.store(42, Ordering::Release);
            FLAG.store(true, Ordering::Release);
        });
        // Implicit drop fires the rollback.
        probe.store(true, Ordering::Release);
    });
    h.join().expect("worker");
    assert!(done.load(Ordering::Acquire), "worker ran");
    // Observer: Acquire on FLAG, then plain read of DATA is ordered.
    let mut spins = 0u32;
    while !FLAG.load(Ordering::Acquire) && spins < 10_000_000 {
        spins += 1;
    }
    assert!(FLAG.load(Ordering::Acquire), "flag arrived");
    assert_eq!(
        DATA.load(Ordering::Acquire),
        42,
        "Release→Acquire carries the rollback write"
    );
}

#[test]
fn guard_handoff_needs_sync_not_luck() {
    // Floor-1 owns NO lock: moving a guard (or its effects) across threads
    // without synchronization is the caller's job. This test documents the
    // shape: channel send/recv (which IS synchronized) carries the guard's
    // effects safely; a raw handoff would not.
    let (tx, rx) = std::sync::mpsc::channel::<u64>();
    let h = std::thread::spawn(move || {
        let _g = SnubberGuard::new(2, 2, || {
            tx.send(77).expect("channel carries the effect");
        });
        // Drop → rollback → send (synchronized by the channel itself).
    });
    h.join().expect("worker");
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("effect"),
        77,
        "synchronized handoff carries the rollback"
    );
}
