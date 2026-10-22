//! Opt-in execution profile: fake clocks, stage attribution, CPU split,
//! and sync caller-parking dwell.

use core::future::Future;
use core::sync::atomic::{AtomicU64, Ordering};
use snubber::{BackpressurePolicy, StageKind, PROFILE_HAS_CPU, PROFILE_STAGES};
use std::sync::Arc;
use std::time::Duration;

static WALL_NS: AtomicU64 = AtomicU64::new(0);
static CPU_NS: AtomicU64 = AtomicU64::new(0);

fn wall() -> u64 {
    WALL_NS.load(Ordering::Relaxed)
}

fn cpu() -> u64 {
    CPU_NS.load(Ordering::Relaxed)
}

#[test]
fn profile_attributes_stages_cpu_and_settle() {
    WALL_NS.store(0, Ordering::Relaxed);
    CPU_NS.store(0, Ordering::Relaxed);
    let node = snubber::SnubberNode::builder("prof_stages").build();

    let mut guard = node
        .enter_profiled_with_clock(wall, Some(cpu), || {})
        .expect("admitted");

    WALL_NS.store(1_000, Ordering::Relaxed); // 1 us of unnamed work
    guard.checkpoint_timed(1, "connect", StageKind::Work);

    WALL_NS.store(501_000, Ordering::Relaxed); // 500 us on "connect"
    CPU_NS.store(100_000, Ordering::Relaxed);
    guard.checkpoint_timed(2, "wait_ack", StageKind::Wait);

    WALL_NS.store(801_000, Ordering::Relaxed); // 300 us waiting
    CPU_NS.store(250_000, Ordering::Relaxed);
    let (event, p) = guard.commit_profiled();

    assert!(matches!(event, snubber::SnubberEvent::Committed { .. }));
    assert_eq!(p.admit_us, 0);
    assert_eq!(p.dwell_us, 0);
    assert_eq!(p.work_wall_us, 801);
    assert_eq!(p.work_cpu_us, 250);
    assert_eq!(p.settle_us, 0);
    assert!(p.has_cpu());
    assert_eq!(p.flags & PROFILE_HAS_CPU, PROFILE_HAS_CPU);
    assert_eq!(p.cpu_utilization_pct(), 31);
    assert!(!p.is_io_starved());
    assert_eq!(p.stage_count, 2);
    assert_eq!(p.stages[0].label, "connect");
    assert_eq!(p.stages[0].wall_us, 500);
    assert_eq!(p.stages[0].kind, StageKind::Work);
    assert_eq!(p.stages[1].label, "wait_ack");
    assert_eq!(p.stages[1].wall_us, 300);
    assert_eq!(p.stages[1].kind, StageKind::Wait);
    assert_eq!(p.slowest_stage().expect("two stages").label, "connect");

    let mut text = String::new();
    p.write_summary(&mut text).expect("fmt::Write cannot fail");
    assert!(text.starts_with("profile admit=0us"));
    assert!(text.contains("connect work 500us"));
    assert!(text.contains("wait_ack wait 300us"));
}

#[test]
fn profile_marks_stage_overflow_and_cpu_starvation() {
    static W2: AtomicU64 = AtomicU64::new(0);
    static C2: AtomicU64 = AtomicU64::new(0);
    fn wall2() -> u64 {
        W2.load(Ordering::Relaxed)
    }
    fn cpu2() -> u64 {
        C2.load(Ordering::Relaxed)
    }

    let node = snubber::SnubberNode::builder("prof_overflow").build();

    let mut guard = node
        .enter_profiled_with_clock(wall2, Some(cpu2), || {})
        .expect("admitted");
    for (i, label) in ["a", "b", "c", "d", "e"].iter().enumerate() {
        W2.store((i as u64 + 1) * 100_000, Ordering::Relaxed);
        guard.checkpoint_timed(i as u32 + 1, label, StageKind::Work);
    }
    W2.store(1_000_000, Ordering::Relaxed);
    C2.store(100_000, Ordering::Relaxed);
    let (_, p) = guard.commit_profiled();

    assert_eq!(p.stage_count as usize, PROFILE_STAGES);
    assert!(p.stage_overflow());
    assert_eq!(p.work_wall_us, 1_000);
    assert_eq!(p.work_cpu_us, 100);
    assert_eq!(p.cpu_utilization_pct(), 10);
    assert!(p.is_io_starved());
}

#[test]
fn plain_guard_returns_zeroed_profile() {
    let node = snubber::SnubberNode::builder("prof_plain").build();
    let guard = node.enter_auto(|| {}).expect("admitted");
    assert!(guard.profile_snapshot().is_none());
    let (event, p) = guard.commit_profiled();
    assert!(matches!(event, snubber::SnubberEvent::Committed { .. }));
    assert_eq!(p, snubber::ExecutionProfile::default());
}

#[test]
fn sync_parking_lands_in_dwell() {
    let node = Arc::new(
        snubber::SnubberNode::builder("prof_dwell")
            .max_in_flight(1)
            .backpressure(BackpressurePolicy::AdaptiveBlock {
                max_wait_ms: 20,
                shed_deeper_than: None,
            })
            .build(),
    );

    let holder = node.enter_auto(|| {}).expect("first admission");
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1));
        holder.commit();
    });

    let guard = node
        .enter_profiled(7, 7, || {})
        .expect("slot freed while parked");
    releaser.join().expect("releaser");

    let wait_ms = guard.queue_wait_ms();
    assert!(wait_ms >= 1, "parked time must be recorded");
    let (_, p) = guard.commit_profiled();
    assert_eq!(p.dwell_us, (wait_ms * 1_000).min(u32::MAX as u64) as u32);
}

struct NoopWake;

impl std::task::Wake for NoopWake {
    fn wake(self: std::sync::Arc<Self>) {}
}

fn noop_waker() -> std::task::Waker {
    std::task::Waker::from(std::sync::Arc::new(NoopWake))
}

#[test]
fn async_profile_arms_on_successful_poll() {
    static AW: AtomicU64 = AtomicU64::new(0);
    static AC: AtomicU64 = AtomicU64::new(0);
    fn aw() -> u64 {
        AW.load(Ordering::Relaxed)
    }
    fn ac() -> u64 {
        AC.load(Ordering::Relaxed)
    }

    let node = snubber::SnubberNode::builder("prof_async").build();
    let mut fut = node.enter_async_profiled_with_clock(1, 1, aw, Some(ac), || {});

    AW.store(1_000, Ordering::Relaxed);
    let waker = noop_waker();
    let mut cx = core::task::Context::from_waker(&waker);
    let mut guard = match core::pin::Pin::new(&mut fut).poll(&mut cx) {
        core::task::Poll::Ready(Ok(g)) => g,
        core::task::Poll::Ready(Err(e)) => panic!("expected admission, got {e:?}"),
        core::task::Poll::Pending => panic!("expected immediate admission, got Pending"),
    };

    AW.store(101_000, Ordering::Relaxed);
    guard.checkpoint_timed(1, "io", StageKind::Wait);
    AW.store(201_000, Ordering::Relaxed);
    AC.store(50_000, Ordering::Relaxed);
    let (_, p) = guard.commit_profiled();

    assert_eq!(p.admit_us, 1);
    assert_eq!(p.work_wall_us, 200);
    assert_eq!(p.work_cpu_us, 50);
    assert_eq!(p.stage_count, 1);
    assert_eq!(p.stages[0].label, "io");
    assert_eq!(p.stages[0].wall_us, 100);
    assert_eq!(p.stages[0].kind, StageKind::Wait);
}

#[test]
fn async_queue_dwell_lands_in_profile() {
    let node = Arc::new(
        snubber::SnubberNode::builder("prof_async_dwell")
            .max_in_flight(1)
            .build(),
    );
    let holder = node.enter_auto(|| {}).expect("first admission");
    let mut fut = node.enter_async_auto_profiled(|| {});

    let waker = noop_waker();
    let mut cx = core::task::Context::from_waker(&waker);
    assert!(core::pin::Pin::new(&mut fut).poll(&mut cx).is_pending());

    std::thread::sleep(Duration::from_millis(3));
    holder.commit();
    let guard = match core::pin::Pin::new(&mut fut).poll(&mut cx) {
        core::task::Poll::Ready(Ok(g)) => g,
        core::task::Poll::Ready(Err(e)) => panic!("expected admission, got {e:?}"),
        core::task::Poll::Pending => panic!("expected admission after slot release, got Pending"),
    };
    let wait_ms = guard.queue_wait_ms();
    assert!(wait_ms >= 1, "async dwell must be recorded");
    let (_, p) = guard.commit_profiled();
    assert_eq!(p.dwell_us, (wait_ms * 1_000).min(u32::MAX as u64) as u32);
}
