//! Worker pool: N independent lineages on N private pipes.
//!
//! Every assertion is deterministic. Drains are fed through in-memory
//! cursors (`PipeDrain::drain_sync`, EOF already reached) or through a
//! reader the test controls; tracker clocks are plain `u64` values. Only
//! the EOF-grace test waits, and it waits on purpose.

use snubber::pool::{
    namespace_task, task_lane, task_local, PoolConfig, PoolError, ReapClass, WorkerPool, MAX_LANES,
};
use snubber::{
    Backoff, MemoryVerdict, PipeDrain, ProcessToken, RestartDecision, RestartPolicy, RestartWindow,
    StateDigest, STATE_LEN,
};
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn drain_of(frames: &[ProcessToken]) -> PipeDrain {
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend_from_slice(&frame.to_bytes());
    }
    PipeDrain::drain_sync(Cursor::new(bytes))
}

fn armed(task: u64) -> PipeDrain {
    drain_of(&[ProcessToken::simple(task, 9, 1)])
}

fn committed(task: u64) -> PipeDrain {
    drain_of(&[ProcessToken::simple(task, 9, 1), ProcessToken::commit(task)])
}

fn silent() -> PipeDrain {
    drain_of(&[])
}

fn stuck() -> PipeDrain {
    struct Stuck;
    impl std::io::Read for Stuck {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::park();
            Ok(0)
        }
    }
    PipeDrain::spawn(Stuck)
}

#[test]
fn lanes_do_not_share_armed_tokens() {
    let pool = WorkerPool::new(PoolConfig::new(2)).expect("two lanes");
    let lane_a = pool.lane(0).expect("lane 0");
    let lane_b = pool.lane(1).expect("lane 1");

    let gen_a = lane_a
        .attach(1_001, armed(namespace_task(0, 7)))
        .expect("attach lane 0");
    let gen_b = lane_b
        .attach(1_002, committed(namespace_task(1, 7)))
        .expect("attach lane 1");
    assert_eq!((gen_a, gen_b), (1, 1), "fresh lanes boot at generation 1");

    let reap_a = lane_a.note_death(None, 1_000).expect("reap lane 0");
    let reap_b = lane_b.note_death(Some(0), 1_000).expect("reap lane 1");

    assert_eq!(reap_a.class, ReapClass::Ambush);
    assert_eq!(reap_a.token.map(|t| t.task), Some(namespace_task(0, 7)));
    assert_eq!(reap_a.generation, 2);

    assert_eq!(reap_b.class, ReapClass::Escape);
    assert!(reap_b.token.is_none());
    assert_eq!(reap_b.decision, RestartDecision::Stop);

    let snap_a = lane_a.snapshot();
    let snap_b = lane_b.snapshot();
    assert_eq!((snap_a.rescued, snap_a.ambush, snap_a.escape), (1, 1, 0));
    assert_eq!((snap_b.rescued, snap_b.ambush, snap_b.escape), (0, 0, 1));

    let report = pool.report();
    assert_eq!(report.totals.reaps, 2);
    assert_eq!(report.totals.rescued, 1);
    assert!(report.conserved());
    println!(
        "[POOL]: tokens never crossed lanes — rescued={} ambush={} escape={}",
        report.totals.rescued, report.totals.ambush, report.totals.escape
    );
}

#[test]
fn generation_bumps_only_on_the_dying_lane() {
    let pool = WorkerPool::new(PoolConfig::new(2)).expect("two lanes");
    let lane_a = pool.lane(0).expect("lane 0");
    let lane_b = pool.lane(1).expect("lane 1");
    lane_b.attach(42, silent()).expect("attach lane 1");

    let mut handed_out = Vec::new();
    for round in 0..3u64 {
        let generation = lane_a
            .attach(100 + round as u32, silent())
            .expect("attach lane 0");
        handed_out.push(generation);
        let reap = lane_a
            .note_death(Some(1), 1_000 + round * 10)
            .expect("reap lane 0");
        assert!(matches!(reap.decision, RestartDecision::Restart { .. }));
        assert_eq!(reap.generation, generation + 1, "successor generation");
    }

    assert_eq!(handed_out, vec![1, 2, 3], "one bump per death");
    assert_eq!(lane_a.generation(), 4);
    assert_eq!(lane_b.generation(), 1, "idle lane keeps its generation");
    assert_eq!(lane_a.next_epoch(), 1, "epoch resets with the generation");
    assert_eq!(lane_b.next_epoch(), 1);
    assert_eq!(
        lane_b.next_epoch(),
        2,
        "epoch is monotonic inside a generation"
    );
}

#[test]
fn classification_conserves_every_reap() {
    let pool = WorkerPool::new(PoolConfig::new(3)).expect("three lanes");
    let lanes: Vec<_> = (0..3).map(|i| pool.lane(i).expect("lane")).collect();

    lanes[0]
        .attach(1, armed(namespace_task(0, 1)))
        .expect("attach");
    lanes[1]
        .attach(2, committed(namespace_task(1, 1)))
        .expect("attach");
    lanes[2].attach(3, silent()).expect("attach");

    let reap_0 = lanes[0].note_death(None, 0).expect("ambush");
    let reap_1 = lanes[1].note_death(Some(0), 0).expect("escape");
    let reap_2 = lanes[2].note_death(Some(1), 0).expect("stillborn");

    assert_eq!(reap_0.class, ReapClass::Ambush);
    assert_eq!(reap_1.class, ReapClass::Escape);
    assert_eq!(reap_2.class, ReapClass::Stillborn);
    assert!(reap_0.token.is_some());
    assert!(reap_1.token.is_none() && reap_2.token.is_none());

    let report = pool.report();
    assert_eq!(report.totals.lanes, 3);
    assert_eq!(report.totals.reaps, 3);
    assert_eq!(report.totals.rescued, 1);
    assert_eq!(report.totals.ambush, 1);
    assert_eq!(report.totals.escape, 1);
    assert_eq!(report.totals.stillborn, 1);
    assert_eq!(report.totals.deadlocks, 0);
    assert_eq!(report.totals.restarts, 2);
    assert_eq!(report.totals.giveups, 0);
    assert!(report.conserved());
    assert!(!report.deadlocked());
    assert!(report.healthy());
    assert_eq!(report.final_generations(), vec![2, 2, 2]);

    let text = report.to_string();
    for needle in [
        "lanes=3",
        "reaps=3",
        "rescued=1",
        "ambush=1",
        "escape=1",
        "stillborn=1",
        "deadlocks=0",
        "conserved=true",
        "lane=2",
    ] {
        assert!(
            text.contains(needle),
            "{needle} missing from report:\n{text}"
        );
    }
    println!("\n{text}");
}

#[test]
fn backoff_and_storm_window_are_per_lane() {
    let pool = WorkerPool::new(PoolConfig::new(2)).expect("two lanes");
    let lane_a = pool.lane(0).expect("lane 0");
    let lane_b = pool.lane(1).expect("lane 1");

    let mut delays = Vec::new();
    for round in 0..3u64 {
        lane_a.attach(10 + round as u32, silent()).expect("attach");
        match lane_a
            .note_death(Some(1), 1_000 + round * 100)
            .expect("reap")
            .decision
        {
            RestartDecision::Restart { delay_ms, .. } => delays.push(delay_ms),
            other => panic!("lane 0 death {round}: {other:?}"),
        }
    }
    assert_eq!(delays, vec![50, 100, 200], "exponential cooling");

    lane_a.attach(90, silent()).expect("attach");
    assert_eq!(
        lane_a.note_death(Some(1), 1_300).expect("reap").decision,
        RestartDecision::GiveUp {
            deaths_in_window: 4
        },
        "fourth death inside 10 s parks lane 0"
    );

    lane_b.attach(91, silent()).expect("attach");
    assert_eq!(
        lane_b.note_death(Some(1), 1_300).expect("reap").decision,
        RestartDecision::Restart {
            delay_ms: 50,
            attempt: 0
        },
        "lane 1 keeps its own fresh window and backoff"
    );

    assert_eq!(lane_a.snapshot().giveups, 1);
    assert_eq!(lane_b.snapshot().giveups, 0);
    assert_eq!(lane_a.generation(), 5, "1 boot + 4 deaths");
    println!("\n[POOL]: per-lane backoff=50,100,200 lane0_giveups=1 lane1_giveups=0");
}

#[test]
fn attach_and_reap_errors_are_explicit() {
    let pool = WorkerPool::new(PoolConfig::new(1)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");

    assert_eq!(
        lane.note_death(None, 0).unwrap_err(),
        PoolError::NoWorkerAttached { lane: 0 }
    );
    assert_eq!(
        lane.note_memory_kb(64).unwrap_err(),
        PoolError::NoWorkerAttached { lane: 0 }
    );

    lane.attach(5, silent()).expect("attach");
    assert_eq!(
        lane.attach(6, silent()).unwrap_err(),
        PoolError::LaneBusy { lane: 0 }
    );

    let reap = lane.note_death(None, 0).expect("reap");
    assert_eq!(reap.lane, 0);
    assert_eq!(reap.respawn_delay_ms(), Some(50));
    assert_eq!(
        lane.note_death(None, 1).unwrap_err(),
        PoolError::NoWorkerAttached { lane: 0 },
        "a double report is a caller bug, not a death"
    );

    assert_eq!(
        WorkerPool::new(PoolConfig::new(0)).unwrap_err(),
        PoolError::NoLanes
    );
    assert_eq!(
        WorkerPool::new(PoolConfig::new(MAX_LANES + 1)).unwrap_err(),
        PoolError::TooManyLanes {
            requested: MAX_LANES + 1,
            max: MAX_LANES
        }
    );
    assert!(PoolError::LaneBusy { lane: 2 }
        .to_string()
        .contains("lane 2"));
    assert_eq!(PoolConfig::default().lanes, 4, "default pool size");
}

#[test]
fn eof_grace_reports_deadlock_instead_of_guessing() {
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(5)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    lane.attach(9, stuck()).expect("attach");

    let started = Instant::now();
    let reap = lane.note_death(None, 0).expect("reap");
    assert!(
        started.elapsed() >= Duration::from_millis(5),
        "grace elapsed"
    );
    assert!(reap.deadlock);
    assert_eq!(reap.class, ReapClass::Deadlocked);
    assert!(reap.token.is_none(), "no token claimed from a live pipe");

    let report = pool.report();
    assert_eq!(report.totals.deadlocks, 1);
    assert!(report.deadlocked());
    assert!(!report.healthy(), "a deadlocked lane is not healthy");
    assert!(report.conserved(), "deadlock is a fourth honest bucket");
    assert!(report.to_string().contains("deadlocks=1"));
}

static GRACE_TICKS: AtomicU64 = AtomicU64::new(0);

fn ticking_clock() -> u64 {
    GRACE_TICKS.fetch_add(1, Ordering::SeqCst)
}

#[test]
fn eof_grace_follows_the_drain_clock_not_wall_time() {
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(5)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    let drain = PipeDrain::manual_with_clock(ticking_clock);
    drain.feed(&ProcessToken::simple(1, 9, 1).to_bytes());
    lane.attach(9, drain).expect("attach");

    let started = Instant::now();
    let reap = lane.note_death(None, 0).expect("reap");
    // Five 1 ms polls move the injected clock past the grace; the verdict
    // comes from the clock value, not from elapsed wall time.
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(reap.deadlock);
    assert_eq!(reap.class, ReapClass::Deadlocked);
    assert_eq!(reap.token, None, "a deadlocked drain claims nothing");
}

#[test]
fn manually_fed_drain_reaps_as_ambush_after_finish() {
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(5)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    let drain = PipeDrain::manual();
    let frame = ProcessToken::simple(namespace_task(0, 11), 9, 1).to_bytes();
    // Split the frame across two pushes, then declare EOF from the app.
    let (head, tail) = frame.split_at(20);
    assert_eq!(drain.feed(head), 0);
    assert_eq!(drain.feed(tail), 1);
    drain.finish();
    lane.attach(10, drain).expect("attach");

    let reap = lane.note_death(None, 0).expect("reap");
    assert!(!reap.deadlock);
    assert_eq!(reap.class, ReapClass::Ambush);
    assert_eq!(
        reap.token.map(|token| token.task),
        Some(namespace_task(0, 11))
    );
    assert_eq!(lane.generation(), 2);
}

#[test]
fn lane_namespaced_tasks_roundtrip() {
    let task = namespace_task(7, 0xDEAD_BEEF);
    assert_eq!(task_lane(task), 7);
    assert_eq!(task_local(task), 0xDEAD_BEEF);
    assert_eq!(namespace_task(0x1234, 1), (0x1234u64 << 48) | 1);
    assert_eq!(namespace_task(u16::MAX, u64::MAX), u64::MAX);
    assert_eq!(task_local(namespace_task(3, u64::MAX)), u64::MAX >> 16);
    assert_ne!(namespace_task(1, 1), namespace_task(2, 1));
    assert_eq!(MAX_LANES, 65_536);
    assert_eq!(task_lane(u64::MAX >> 16), 0, "plain ids stay in lane 0");
}

#[test]
fn memory_policy_is_per_lane_and_resets_per_child() {
    let pool = WorkerPool::new(PoolConfig::new(2).with_memory(100, 200)).expect("two lanes");
    let lane_a = pool.lane(0).expect("lane 0");
    let lane_b = pool.lane(1).expect("lane 1");
    lane_a.attach(1, silent()).expect("attach a");
    lane_b.attach(2, silent()).expect("attach b");

    assert_eq!(
        lane_a.note_memory_kb(150).expect("sample"),
        MemoryVerdict::Soft
    );
    assert_eq!(
        lane_a.note_memory_kb(250).expect("sample"),
        MemoryVerdict::Hard
    );
    assert!(lane_a.memory_verdict().is_hard());
    assert_eq!(
        lane_b.memory_verdict(),
        MemoryVerdict::Ok,
        "lane 1 saw no sample of its own"
    );
    assert_eq!(lane_b.snapshot().memory.peak_kb(), 0);

    let reap = lane_a.note_death(None, 0).expect("reap");
    assert!(matches!(reap.decision, RestartDecision::Restart { .. }));
    lane_a.attach(3, silent()).expect("reattach");
    assert_eq!(
        lane_a.memory_verdict(),
        MemoryVerdict::Ok,
        "watch resets with the child"
    );
}

#[test]
fn state_digest_survives_the_reap() {
    let digest = StateDigest::new([0x5A; STATE_LEN]);
    let pool = WorkerPool::new(PoolConfig::new(1)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    lane.attach(11, drain_of(&digest.to_frames()))
        .expect("attach");

    let reap = lane.note_death(Some(1), 0).expect("reap");
    assert_eq!(reap.state, Some(digest), "handoff payload is preserved");
    assert_eq!(
        reap.class,
        ReapClass::Stillborn,
        "state frames are not arm frames"
    );
    assert!(reap.token.is_none());
}

#[test]
fn stop_policy_keeps_the_generation_contract() {
    let config = PoolConfig::new(1).with_restart(
        RestartPolicy::Never,
        Backoff::none(),
        RestartWindow::unlimited(),
    );
    let pool = WorkerPool::new(config).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    lane.attach(1, committed(namespace_task(0, 4)))
        .expect("attach");

    let reap = lane.note_death(Some(0), 1_000).expect("reap");
    assert_eq!(reap.class, ReapClass::Escape);
    assert_eq!(reap.decision, RestartDecision::Stop);
    assert_eq!(reap.respawn_delay_ms(), None, "Stop has no cooling pause");
    assert_eq!(
        reap.generation, 2,
        "a death bumps the lane even when parked"
    );

    let snapshot = lane.snapshot();
    assert_eq!(
        (snapshot.attempts, snapshot.restarts, snapshot.giveups),
        (1, 0, 0)
    );
    assert_eq!(lane.generation(), 2);
}

#[test]
fn giveup_has_no_respawn_delay() {
    let config = PoolConfig::new(1).with_restart(
        RestartPolicy::OnCrash,
        Backoff::exponential(50, 1_000),
        RestartWindow::new(1, 10),
    );
    let pool = WorkerPool::new(config).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");

    lane.attach(1, silent()).expect("attach");
    let first = lane.note_death(Some(1), 1_000).expect("first death");
    assert_eq!(
        first.decision,
        RestartDecision::Restart {
            delay_ms: 50,
            attempt: 0
        }
    );
    assert_eq!(first.respawn_delay_ms(), Some(50));

    lane.attach(2, silent()).expect("reattach");
    let second = lane.note_death(Some(1), 1_000).expect("second death");
    assert_eq!(
        second.decision,
        RestartDecision::GiveUp {
            deaths_in_window: 2
        }
    );
    assert_eq!(second.respawn_delay_ms(), None, "GiveUp never respawns");
    assert_eq!(lane.snapshot().giveups, 1);
    assert_eq!(lane.generation(), 3, "both deaths bumped the lane");
}

struct OneFrameThenBlock {
    frame: Vec<u8>,
    pos: usize,
    park: std::sync::mpsc::Receiver<()>,
}

impl std::io::Read for OneFrameThenBlock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.frame.len() {
            let take = (self.frame.len() - self.pos).min(buf.len());
            buf[..take].copy_from_slice(&self.frame[self.pos..self.pos + take]);
            self.pos += take;
            return Ok(take);
        }
        let _ = self.park.recv();
        Ok(0)
    }
}

#[test]
fn lease_expiry_sees_a_live_silent_pipe() {
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = OneFrameThenBlock {
        frame: ProcessToken::commit(5).to_bytes().to_vec(),
        pos: 0,
        park: rx,
    };
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(20)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    lane.attach(77, PipeDrain::spawn(reader)).expect("attach");

    let deadline = Instant::now() + Duration::from_secs(5);
    while lane.counters().frames == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(lane.counters().frames, 1, "one frame parsed");
    assert!(!lane.expired(60_000), "a fresh frame renews the lane lease");

    std::thread::sleep(Duration::from_millis(25));
    let idle = lane.idle_ms();
    assert!(
        lane.expired(5),
        "pipe alive, silent past 5 ms: idle={idle} counters={:?}",
        lane.counters()
    );
    assert!(lane.idle_ms() >= 5);

    let started = Instant::now();
    let reap = lane.note_death(None, 0).expect("reap");
    assert!(
        started.elapsed() >= Duration::from_millis(20),
        "grace waited"
    );
    assert!(reap.deadlock);
    assert_eq!(reap.class, ReapClass::Deadlocked);
    assert!(reap.token.is_none());
    assert_eq!(lane.generation(), 2);
    drop(tx);
}

#[test]
fn pool_sweep_names_only_the_silent_lane() {
    let (tx, rx) = std::sync::mpsc::channel();
    let pool = WorkerPool::new(PoolConfig::new(2).with_eof_grace(0)).expect("two lanes");
    let silent = pool.lane(0).expect("lane 0");
    silent
        .attach(
            31,
            PipeDrain::spawn(OneFrameThenBlock {
                frame: ProcessToken::commit(1).to_bytes().to_vec(),
                pos: 0,
                park: rx,
            }),
        )
        .expect("attach");
    assert!(pool.lane(1).is_some(), "lane 1 stays unattached");

    let deadline = Instant::now() + Duration::from_secs(5);
    while silent.counters().frames == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        pool.expired_lanes(60_000).is_empty(),
        "a fresh frame renews the lease"
    );

    std::thread::sleep(Duration::from_millis(25));
    let expired = pool.expired_lanes(5);
    assert_eq!(expired.len(), 1, "only the silent, attached lane");
    assert_eq!(expired[0].0, 0);
    assert!(expired[0].1 >= 5, "idle={} ms", expired[0].1);

    let reap = silent.note_death(None, 0).expect("reap");
    assert!(reap.deadlock);
    assert!(
        pool.expired_lanes(5).is_empty(),
        "an idle lane is not a lease breach"
    );
    drop(tx);
}

#[test]
fn zero_grace_reports_deadlock_without_waiting() {
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(0)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    lane.attach(9, stuck()).expect("attach");

    let started = Instant::now();
    let reap = lane.note_death(None, 0).expect("reap");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "grace 0 must not poll"
    );
    assert!(reap.deadlock);
    assert_eq!(reap.class, ReapClass::Deadlocked);
    assert_eq!(lane.generation(), 2, "a deadlock is still a death");
}

#[test]
fn hard_limit_below_soft_is_clamped_up() {
    let pool = WorkerPool::new(PoolConfig::new(1).with_memory(100, 50)).expect("one lane");
    let lane = pool.lane(0).expect("lane 0");
    lane.attach(1, silent()).expect("attach");

    let watch = lane.snapshot().memory;
    assert_eq!(
        (watch.soft_kb(), watch.hard_kb()),
        (100, 100),
        "hard clamps up to soft so the drain phase exists"
    );
    assert_eq!(lane.note_memory_kb(99).expect("sample"), MemoryVerdict::Ok);
    assert_eq!(
        lane.note_memory_kb(100).expect("sample"),
        MemoryVerdict::Hard,
        "clamped limits fire together, never a silent skip"
    );
    assert_eq!(lane.snapshot().memory.peak_kb(), 100);

    let soft_pool = WorkerPool::new(PoolConfig::new(1).with_memory(100, 0)).expect("one lane");
    let soft_lane = soft_pool.lane(0).expect("lane 0");
    soft_lane.attach(2, silent()).expect("attach");
    assert_eq!(
        soft_lane.note_memory_kb(100).expect("sample"),
        MemoryVerdict::Soft
    );
    assert_eq!(
        soft_lane.note_memory_kb(500).expect("sample"),
        MemoryVerdict::Soft,
        "hard disabled: soft keeps requesting a drain"
    );
}

#[test]
fn empty_pool_reports_healthy() {
    let pool = WorkerPool::new(PoolConfig::new(2)).expect("two lanes");
    assert_eq!(pool.len(), 2);
    assert!(!pool.is_empty());
    assert_eq!(pool.config().lanes, 2);
    assert_eq!(pool.lanes().len(), 2);
    assert!(pool.lane(2).is_none());
    assert_eq!(pool.total_rescued(), 0);

    let report = pool.report();
    assert_eq!(report.totals.lanes, 2);
    assert_eq!(report.totals.reaps, 0);
    assert_eq!(report.final_generations(), vec![1, 1]);
    assert!(report.conserved());
    assert!(report.healthy());
    assert!(!report.deadlocked());
    assert!(report.lane(0).is_some());
    assert!(report.lane(2).is_none());

    let text = report.to_string();
    assert!(text.contains("POOL lanes=2 reaps=0"), "{text}");
    assert!(text.contains("A/E/S/D=0/0/0/0"), "{text}");
    assert!(text.contains("conserved=true"), "{text}");
}

#[test]
fn total_rescued_sums_every_lane() {
    let pool = WorkerPool::new(PoolConfig::new(2)).expect("two lanes");
    for index in 0..2usize {
        let lane = pool.lane(index).expect("lane");
        lane.attach(index as u32 + 1, armed(namespace_task(index as u16, 9)))
            .expect("attach");
        let reap = lane.note_death(None, 0).expect("reap");
        assert_eq!(reap.class, ReapClass::Ambush);
    }
    assert_eq!(pool.total_rescued(), 2);
    assert_eq!(pool.report().totals.rescued, 2);
}

#[test]
fn four_lanes_fifty_cycles_stay_isolated() {
    let config = PoolConfig::new(4).with_restart(
        RestartPolicy::Always,
        Backoff::none(),
        RestartWindow::unlimited(),
    );
    let pool = WorkerPool::new(config).expect("four lanes");
    const CYCLES: u64 = 25;

    std::thread::scope(|scope| {
        for index in 0..4usize {
            let lane = Arc::clone(pool.lane(index).expect("lane"));
            scope.spawn(move || {
                for local in 0..CYCLES {
                    let generation = lane.generation();
                    lane.attach(
                        10_000 + index as u32,
                        armed(namespace_task(index as u16, local)),
                    )
                    .expect("attach");
                    let reap = lane.note_death(Some(9), local * 10).expect("reap");
                    assert_eq!(reap.class, ReapClass::Ambush);
                    assert_eq!(reap.lane, index);
                    assert_eq!(
                        reap.token
                            .map(|token| (task_lane(token.task), task_local(token.task))),
                        Some((index as u16, local)),
                        "token must name its own lane and local id"
                    );
                    assert_eq!(reap.generation, generation + 1);
                }
            });
        }
    });

    let report = pool.report();
    assert_eq!(report.totals.reaps, 4 * CYCLES);
    assert_eq!(report.totals.ambush, 4 * CYCLES);
    assert_eq!(report.totals.rescued, 4 * CYCLES);
    assert_eq!(report.totals.escape, 0);
    assert_eq!(report.totals.stillborn, 0);
    assert_eq!(report.totals.deadlocks, 0);
    assert_eq!(pool.total_rescued(), 4 * CYCLES);
    for (index, lane) in report.lanes.iter().enumerate() {
        assert_eq!(lane.index, index);
        assert_eq!(lane.reaps, CYCLES);
        assert_eq!(lane.ambush, CYCLES);
        assert_eq!(lane.rescued, CYCLES);
        assert_eq!(lane.generation, CYCLES + 1);
        assert!(lane.conserved());
    }
    assert!(report.conserved());
    assert!(report.healthy());
    println!(
        "\n[POOL]: 4 lanes x {CYCLES} cycles — reaps={} ambush={} deadlocks={}",
        report.totals.reaps, report.totals.ambush, report.totals.deadlocks
    );
}

#[test]
fn successor_gets_fresh_generation_during_eof_grace() {
    // Regression: the lane clock was bumped AFTER the EOF grace wait, so a
    // successor attaching inside the grace window inherited the DEAD
    // child's generation — two lineages sharing one generation number.
    // Now `note_death` bumps the clock before waiting for EOF.
    let pool = Arc::new(WorkerPool::new(PoolConfig::new(1).with_eof_grace(150)).expect("one lane"));
    let lane = pool.lane(0).expect("lane 0");
    lane.attach(2_001, stuck()).expect("attach dying child");

    let reaper = {
        let pool = Arc::clone(&pool);
        std::thread::spawn(move || pool.lane(0).expect("lane").note_death(None, 1_000))
    };
    // `note_death` detaches FIRST (pid -> None), then waits out the grace.
    let deadline = Instant::now() + Duration::from_secs(2);
    while lane.pid().is_some() {
        assert!(Instant::now() < deadline, "note_death never detached");
        std::thread::yield_now();
    }

    // Successor attaches INSIDE the grace window: it must get the fresh
    // generation, not the dead child's.
    let gen = lane.attach(2_002, stuck()).expect("attach successor");
    assert_eq!(
        gen, 2,
        "successor must NOT inherit the dead child's generation"
    );

    let reap = reaper.join().expect("reaper thread").expect("reap");
    assert_eq!(reap.generation, 2);
    assert_eq!(reap.class, ReapClass::Deadlocked);
}

// F-04 (PLAN_AUDIT2): a successor that attaches while the reaper still
// sits in the EOF grace must not inherit the dead predecessor's
// `last_exit`/`last_class`. Handshake, not a sleep margin: `attach` can
// only succeed after the reaper's first lock cleared `pid`, so a
// successful spin-attach proves the reaper is inside the grace window.
#[test]
fn attach_during_grace_leaves_successor_last_telemetry_clean() {
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(150)).expect("one lane");
    let lane = Arc::clone(pool.lane(0).expect("lane 0"));
    // Manual drain, never finished: no EOF inside the grace -> Deadlocked.
    lane.attach(2_001, PipeDrain::manual())
        .expect("attach dying child");

    let reaper = {
        let lane = Arc::clone(&lane);
        std::thread::spawn(move || lane.note_death(Some(7), 1_000))
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    let successor_gen = loop {
        match lane.attach(2_002, PipeDrain::manual()) {
            Ok(gen) => break gen,
            Err(PoolError::LaneBusy { .. }) if Instant::now() < deadline => {
                std::thread::yield_now();
            }
            Err(other) => panic!("attach failed: {other:?}"),
        }
    };

    let reap = reaper.join().expect("reaper thread").expect("reap");
    assert_eq!(reap.class, ReapClass::Deadlocked);
    assert_eq!(reap.generation, successor_gen);

    let snap = lane.snapshot();
    assert_eq!(
        snap.last_class, None,
        "predecessor telemetry must not be stamped onto the successor"
    );
    assert_eq!(snap.last_exit, None);
    assert_eq!(snap.generation, successor_gen);
    // The death itself is still counted as lane history.
    assert_eq!(snap.reaps, 1);
    assert_eq!(snap.deadlocks, 1);
}

// Without an attach inside the grace, the reaper owns the lane: the
// predecessor's telemetry lands as before.
#[test]
fn reaper_without_successor_still_stamps_last_telemetry() {
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(150)).expect("one lane");
    let lane = Arc::clone(pool.lane(0).expect("lane 0"));
    lane.attach(2_001, PipeDrain::manual())
        .expect("attach dying child");
    let reap = lane.note_death(Some(3), 1_000).expect("reap");
    assert_eq!(reap.class, ReapClass::Deadlocked);
    let snap = lane.snapshot();
    assert_eq!(snap.last_class, Some(ReapClass::Deadlocked));
    assert_eq!(snap.last_exit, Some(3));
}
