//! Newton's 3rd law: the wall pushes back with MEASURED resistance.
//! AdviseOnly reads, AdaptiveBlock parks, Custom reacts, friction bends.

use snubber::{BackpressurePolicy, EnterError, FaultKind, SnubberNode};

#[test]
fn pressure_readings_come_with_refusal() {
    let node = SnubberNode::builder("pace/advise").max_in_flight(1).build(); // AdviseOnly default
    let _held = node.enter(1, 1, || {}).expect("first slot");
    match node.enter(2, 2, || {}) {
        Err(e) => {
            let bp = e.backpressure().expect("bulkhead IS pressure");
            assert_eq!(bp.kind, FaultKind::BulkheadFull);
            assert!(bp.depth > 0, "depth measured: {}", bp.depth);
            assert!(bp.retry_after_ms > 0, "wait advised");
            assert_eq!(e.retry_after_ms(), bp.retry_after_ms);
        }
        Ok(_) => panic!("second must be refused"),
    }
}

#[test]
fn stale_and_poison_carry_no_pressure() {
    let node = SnubberNode::builder("pace/none").build();
    node.enter(5, 1, || {}).map(|g| g.commit()).expect("arm");
    match node.enter(3, 1, || {}) {
        Err(e) => {
            assert!(e.backpressure().is_none(), "stale is not pressure");
            assert_eq!(e.retry_after_ms(), 0);
        }
        Ok(_) => panic!("replay must be cut"),
    }
    assert!(EnterError::BreakerOpen.backpressure().is_some());
    assert!(EnterError::RateLimited.backpressure().is_some());
}

#[test]
fn adaptive_block_parks_on_friction_then_admits() {
    let node = SnubberNode::builder("pace/adaptive")
        .max_in_flight(1)
        .backpressure(BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 100,
            shed_deeper_than: None,
        })
        .build();
    let probe = node.enter(1, 1, || {}).expect("first slot");
    let n2 = node.clone();
    let parked = std::thread::spawn(move || n2.enter(2, 2, || {}));
    std::thread::sleep(std::time::Duration::from_millis(10));
    probe.commit();
    parked
        .join()
        .expect("thread")
        .expect("admitted after friction park")
        .commit();
    assert_eq!(node.stats().committed, 2);
}

#[test]
fn shed_cap_rejects_without_parking() {
    // depth at full bulkhead is 255 → cap 128 sheds immediately.
    let node = SnubberNode::builder("pace/shed")
        .max_in_flight(1)
        .backpressure(BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 500,
            shed_deeper_than: Some(128),
        })
        .build();
    let _held = node.enter(1, 1, || {}).expect("first slot");
    let start = std::time::Instant::now();
    let r = node.enter(2, 2, || {});
    assert!(
        matches!(r, Err(EnterError::BulkheadFull)),
        "shed, not parked"
    );
    assert!(start.elapsed().as_millis() < 100, "no sleep on shed path");
}

#[test]
fn custom_reaction_fires_with_readings() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEEN_DEPTH: AtomicU64 = AtomicU64::new(0);
    fn reporter(bp: snubber::Backpressure) {
        SEEN_DEPTH.store(bp.depth as u64, Ordering::SeqCst);
    }
    let node = SnubberNode::builder("pace/custom")
        .max_in_flight(1)
        .backpressure(BackpressurePolicy::Custom(reporter))
        .build();
    let _held = node.enter(1, 1, || {}).expect("first slot");
    let r = node.enter(2, 2, || {});
    assert!(matches!(r, Err(EnterError::BulkheadFull)));
    assert!(SEEN_DEPTH.load(Ordering::SeqCst) > 0, "reporter saw depth");
}

#[test]
fn custom_friction_bends_the_spring() {
    // Quadratic curve: full bulkhead of 1 → 10 + 500 = 510ms advised.
    fn quad(in_flight: u32, max: u32) -> u64 {
        let r = in_flight as f32 / max.max(1) as f32;
        (10.0 + r * r * 500.0) as u64
    }
    let node = SnubberNode::builder("pace/friction")
        .max_in_flight(1)
        .friction_fn(quad)
        .backpressure(BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 5, // cap clamps the 510ms curve to 5ms
            shed_deeper_than: None,
        })
        .build();
    let _held = node.enter(1, 1, || {}).expect("first slot");
    let start = std::time::Instant::now();
    let r = node.enter(2, 2, || {});
    assert!(matches!(r, Err(EnterError::BulkheadFull)));
    let ms = start.elapsed().as_millis();
    assert!((4..=60).contains(&ms), "capped friction park: {ms}ms");
}
