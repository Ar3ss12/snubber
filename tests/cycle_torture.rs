//! Cycle torture: 8 threads x 5k enter/commit/drop/report.
//!
//! CAMP 1 — CONSERVATION (this file): all reflexes OFF on purpose.
//! Proves the iron lets go: every admission settles exactly once,
//! no slot leaks, no poison, no deadlock. Wide bulkhead, breaker OFF,
//! Panic policy disarmed (trip behavior lives in CAMP 2).
//! See `tests/reflex_breaker.rs` + `tests/reflex_bulkhead.rs` for CAMP 2
//! (reflexes ON: the brakes must slam, not swallow).
//!
//! F3 CONTRACT: the bounded stale-retry loop BELOW lives ONLY here (storm
//! imitation for 8 racing threads). In-crate `StaleEpoch` stays a HARD
//! `Err` — never move this retry into `snubber` itself.
//!
//! Honest accounting: global epoch allocator, every Err counted by kind,
//! REAL rollback counter, full conservation — no `1 = 1+0+0` illusions.

use snubber::{EnterError, FaultKind, SnubberNode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const THREADS: u64 = 8;
const PER_THREAD: u64 = 5_000;

static ROLLBACK_FIRED: AtomicU64 = AtomicU64::new(0);
static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static BULKHEAD_HITS: AtomicU64 = AtomicU64::new(0);
static STALE_HITS: AtomicU64 = AtomicU64::new(0);
static BREAKER_HITS: AtomicU64 = AtomicU64::new(0);
static RATE_HITS: AtomicU64 = AtomicU64::new(0);
static REPORTS_OK: AtomicU64 = AtomicU64::new(0);

fn rollback() {
    ROLLBACK_FIRED.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn cycle_torture_conservation_holds() {
    ROLLBACK_FIRED.store(0, Ordering::SeqCst);
    ATTEMPTS.store(0, Ordering::SeqCst);
    BULKHEAD_HITS.store(0, Ordering::SeqCst);
    STALE_HITS.store(0, Ordering::SeqCst);
    BREAKER_HITS.store(0, Ordering::SeqCst);
    RATE_HITS.store(0, Ordering::SeqCst);
    REPORTS_OK.store(0, Ordering::SeqCst);
    let node = Arc::new(
        SnubberNode::builder("torture/cycle")
            .max_in_flight(64)
            .queue_limit(65536)
            .trip_after_failures(0)
            .policy(
                FaultKind::Panic,
                snubber::FaultPolicy::arm(
                    snubber::FaultAction::DiscardSilently,
                    snubber::LogLevel::Critical,
                    "torture reports panic without isolation",
                    "torture_panics_total",
                ),
            )
            .on_fault(|_, _, _| panic!("telemetry plugin died"))
            .build(),
    );
    let mut handles = Vec::new();
    let epoch_alloc = Arc::new(AtomicU64::new(1));
    for t in 0..THREADS {
        let n = Arc::clone(&node);
        let alloc = Arc::clone(&epoch_alloc);
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                let task_base = (t * PER_THREAD + i) * 16;
                let mut retry = 0u64;
                let mut admitted_epoch = 0u64;
                loop {
                    let e = alloc.fetch_add(1, Ordering::SeqCst);
                    let task = task_base + retry;
                    ATTEMPTS.fetch_add(1, Ordering::SeqCst);
                    match n.enter(e, task, rollback) {
                        Ok(g) => {
                            admitted_epoch = e;
                            match i % 3 {
                                0 => {
                                    g.commit();
                                }
                                1 => {
                                    g.abort("planned");
                                }
                                _ => drop(g),
                            }
                            break;
                        }
                        Err(EnterError::StaleEpoch) => {
                            STALE_HITS.fetch_add(1, Ordering::SeqCst);
                            retry += 1;
                            if retry >= 64 {
                                break;
                            }
                            continue;
                        }
                        Err(EnterError::BulkheadFull) => {
                            BULKHEAD_HITS.fetch_add(1, Ordering::SeqCst);
                            break;
                        }
                        Err(EnterError::BreakerOpen) => {
                            BREAKER_HITS.fetch_add(1, Ordering::SeqCst);
                            break;
                        }
                        Err(EnterError::RateLimited) => {
                            RATE_HITS.fetch_add(1, Ordering::SeqCst);
                            break;
                        }
                        Err(EnterError::InvariantBreach(_))
                        | Err(EnterError::PayloadBudgetExceeded(_)) => {
                            unreachable!("plain enter cannot breach");
                        }
                        Err(EnterError::QueueTtlExpired) | Err(EnterError::QueueFull) => {
                            BULKHEAD_HITS.fetch_add(1, Ordering::SeqCst);
                            break;
                        }
                    }
                }
                if i % 97 == 0 {
                    let rep_epoch = alloc.fetch_add(1, Ordering::SeqCst);
                    n.report(FaultKind::Panic, rep_epoch, u64::MAX - task_base - i);
                    REPORTS_OK.fetch_add(1, Ordering::SeqCst);
                    let _ = admitted_epoch;
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("no thread wedged");
    }
    let s = node.stats();
    let attempts = ATTEMPTS.load(Ordering::SeqCst);
    let bulk = BULKHEAD_HITS.load(Ordering::SeqCst);
    let stale = STALE_HITS.load(Ordering::SeqCst);
    let brk = BREAKER_HITS.load(Ordering::SeqCst);
    let rate = RATE_HITS.load(Ordering::SeqCst);
    let rollbacks = ROLLBACK_FIRED.load(Ordering::SeqCst);
    let reports = REPORTS_OK.load(Ordering::SeqCst);
    let settled = s.committed + s.explicit_aborts + s.implicit_drops;
    println!(
        "\n[REALITY]: attempts={} started={} committed={} aborts={} drops={} bulkhead={}({}) stale={}({}) breaker={} rate={} rollbacks={} reports={}(node={}) poison={}",
        attempts,
        s.total_started,
        s.committed,
        s.explicit_aborts,
        s.implicit_drops,
        bulk,
        s.bulkhead_rejections,
        stale,
        s.stale_epochs_rejected,
        brk,
        rate,
        rollbacks,
        reports,
        s.reported[FaultKind::Panic.index()],
        s.poison_recoveries,
    );
    assert_eq!(
        attempts,
        s.total_started + bulk + stale + brk + rate,
        "every attempt admitted or rejected by kind"
    );
    assert_eq!(bulk, s.bulkhead_rejections, "bulkhead books match");
    assert_eq!(stale, s.stale_epochs_rejected, "stale books match");
    assert_eq!(
        s.total_started, settled,
        "conservation: started == committed + aborts + drops"
    );
    // Throughput: every LOGICAL item (8x5000) admitted at least once.
    // Stale hits are race retries (fresh epoch re-issued), bulkhead must
    // still be zero on a wide node, breaker/rate OFF.
    assert_eq!(bulk, 0, "wide bulkhead must admit everything");
    assert_eq!(brk, 0, "breaker OFF must never fast-reject");
    assert_eq!(rate, 0, "no rate limit armed");
    assert_eq!(
        s.total_started,
        THREADS * PER_THREAD,
        "all 40000 logical items really entered"
    );
    assert_eq!(
        rollbacks,
        s.explicit_aborts + s.implicit_drops,
        "abort + drop discharge hardware exactly once each"
    );
    assert_eq!(s.poison_recoveries, 0, "zero-panic under torture");
    assert_eq!(
        reports,
        s.reported[FaultKind::Panic.index()],
        "every explicit report counted"
    );
}
