//! Pattern 08: Hot Standby Failover (Zero-Framework N+1 Pool)
//!
//! Run: `cargo run --example 08_hot_standby_failover`
//!
//! In-memory state-machine simulation of an N+1 Hot Standby pool.
//! Shows deterministic slot failover logic without spawning real OS child processes:
//! - Primitives used: `GenerationClock`, `RestartTracker`, `StateDigest`, `Priority`.
//! - Flat `[WorkerSlot; N]` array, N+1 standby spare.
//! - Round-robin over free slots, dead worker reclamation without halting the host.
//! - Demonstrates zero-downtime state digest handoff from dead primary to warm standby.

use snubber::{
    Backoff, GenerationClock, RestartPolicy, RestartTracker, RestartWindow, StateDigest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Active,
    Standby,
    Dead,
}

#[derive(Debug)]
struct WorkerSlot {
    #[allow(dead_code)]
    id: usize,
    state: SlotState,
    gen: u64,
    digest: StateDigest,
}

impl WorkerSlot {
    fn free(id: usize) -> Self {
        Self {
            id,
            state: SlotState::Free,
            gen: 0,
            digest: StateDigest::default(),
        }
    }
}

fn main() {
    // Pool of 4: flat array, no Vec growth, no framework.
    let mut pool = [
        WorkerSlot::free(0),
        WorkerSlot::free(1),
        WorkerSlot::free(2),
        WorkerSlot::free(3),
    ];
    let mut clock = GenerationClock::new();

    // Boot: slot 0 Active, slot 1 Standby (warm, pre-warmed spare).
    pool[0].state = SlotState::Active;
    pool[0].gen = clock.generation();
    pool[1].state = SlotState::Standby;
    pool[1].gen = clock.generation();
    println!("POOL: active=0 standby=1 free=2,3");

    // Active publishes state, then dies (simulated process crash).
    let mut live = [0u8; 64];
    live[0] = 42;
    pool[0].digest = StateDigest::new(live);
    pool[0].state = SlotState::Dead;
    println!("POOL: slot 0 died mid-frame, digest kept");

    // Failover in 0 ms: standby spare takes the token + digest, no respawn wait.
    let kept = pool[0].digest;
    pool[1].state = SlotState::Active;
    pool[1].digest = kept;
    assert_eq!(pool[1].digest.bytes[0], 42);
    println!("POOL: standby 1 -> active, mark=42, downtime=0ms");
    // Tracker authorizes replacement process spawn in background.
    let mut tracker = RestartTracker::new(
        RestartPolicy::OnCrash,
        Backoff::exponential(50, 1000),
        RestartWindow::new(3, 10),
    );
    match tracker.note_death(Some(1), 1000) {
        snubber::RestartDecision::Restart { delay_ms, attempt } => {
            println!("POOL: replacement authorized attempt={attempt} delay={delay_ms}ms");
        }
        other => panic!("must restart: {other:?}"),
    }
    // New standby rises: gen+1, inherits digest on tick 0.
    let g = clock.restart();
    pool[0] = WorkerSlot::free(0);
    pool[0].state = SlotState::Standby;
    pool[0].gen = g;
    pool[0].digest = kept;
    // Round-robin over free slots: 10 jobs across 3 live slots.
    let mut rr = 0usize;
    let mut done = 0u32;
    for _ in 0..10 {
        let mut placed = false;
        for _ in 0..pool.len() {
            let s = &mut pool[rr % pool.len()];
            rr += 1;
            if s.state == SlotState::Active || s.state == SlotState::Standby {
                done += 1;
                placed = true;
                break;
            }
        }
        assert!(placed, "conveyor never stops");
    }
    assert_eq!(done, 10);
    println!("POOL: 10 jobs over live slots, conveyor unbroken, N+1 ready");
}
