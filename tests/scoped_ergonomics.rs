//! Verification of ergonomic scoped execution & universal snub_check! macro.
//! Tests scenarios across Databases, Financial Orders, CNC Robotics, and Audio DSP.

use snubber::{snub_check, EnterError, SnubberError, SnubberNode};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn scoped_success_commits_automatically() {
    let node = SnubberNode::builder("db_engine").build();
    let rollback_fired = Arc::new(AtomicBool::new(false));
    let rb = Arc::clone(&rollback_fired);

    let res: Result<u64, SnubberError<&'static str>> = node.scoped(
        move || {
            rb.store(true, Ordering::SeqCst);
        },
        || {
            // Successful transaction
            Ok(999)
        },
    );

    assert_eq!(res.unwrap(), 999);
    assert!(
        !rollback_fired.load(Ordering::SeqCst),
        "Rollback must NOT fire on successful commit"
    );

    let stats = node.stats();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.implicit_drops, 0);
}

#[test]
fn scoped_err_triggers_rollback_and_surfaces_work_error() {
    let node = SnubberNode::builder("robotics_arm").build();
    let brake_applied = Arc::new(AtomicBool::new(false));
    let rb = Arc::clone(&brake_applied);

    #[derive(Debug, PartialEq)]
    enum MotorFault {
        OverCurrent,
    }

    let res: Result<(), SnubberError<MotorFault>> = node.scoped(
        move || {
            rb.store(true, Ordering::SeqCst);
        },
        || {
            // Motor failed
            Err(MotorFault::OverCurrent)
        },
    );

    match res {
        Err(SnubberError::Work(MotorFault::OverCurrent)) => {}
        other => panic!("expected Work(OverCurrent), got: {:?}", other),
    }

    assert!(
        brake_applied.load(Ordering::SeqCst),
        "Brake/rollback MUST be executed via RAII when work fails"
    );

    let stats = node.stats();
    assert_eq!(stats.committed, 0);
    assert_eq!(stats.implicit_drops, 1); // Tracked as implicit drop (rollback executed)
}

#[test]
fn scoped_panic_triggers_rollback_via_unwind() {
    let node = SnubberNode::builder("audio_dac").build();
    let muted = Arc::new(AtomicBool::new(false));
    let rb = Arc::clone(&muted);

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = node.scoped::<(), &'static str, _, _>(
            move || {
                rb.store(true, Ordering::SeqCst);
            },
            || {
                panic!("DSP division by zero!");
            },
        );
    }));

    assert!(res.is_err(), "Panic must unwind");
    assert!(
        muted.load(Ordering::SeqCst),
        "Audio DAC must be muted on panic during transaction"
    );

    let stats = node.stats();
    assert_eq!(stats.committed, 0);
    assert_eq!(stats.implicit_drops, 1);
}

#[test]
fn scoped_checked_rejects_before_admission_with_snub_check() {
    let node = SnubberNode::builder("crypto_exchange").build();
    let rollback_fired = Arc::new(AtomicBool::new(false));
    let rb = Arc::clone(&rollback_fired);

    struct Order {
        amount_sats: i64,
        fee_sats: i64,
    }

    let bad_order = Order {
        amount_sats: -500, // Invalid negative amount
        fee_sats: 10,
    };

    let executed = Arc::new(AtomicBool::new(false));
    let ex = Arc::clone(&executed);

    let res: Result<u64, SnubberError<&'static str>> = node.scoped_checked(
        &bad_order,
        |ord| {
            snub_check!(ord.amount_sats > 0, "amount must be positive");
            snub_check!(ord.fee_sats >= 0, "fee must not be negative");
            Ok(())
        },
        move || {
            rb.store(true, Ordering::SeqCst);
        },
        move |ord| {
            ex.store(true, Ordering::SeqCst);
            Ok(ord.amount_sats as u64)
        },
    );

    match res {
        Err(SnubberError::Admission(EnterError::InvariantBreach("amount must be positive"))) => {}
        other => panic!("expected InvariantBreach error, got: {:?}", other),
    }

    assert!(
        !executed.load(Ordering::SeqCst),
        "Protected work must NEVER run if invariant fails"
    );
    assert!(
        !rollback_fired.load(Ordering::SeqCst),
        "Rollback must NOT run because resource was never armed"
    );

    let stats = node.stats();
    assert_eq!(stats.total_started, 0, "Gate never admitted bad order");
    assert_eq!(stats.poisoned_rejected, 1);
}

#[test]
fn scoped_checked_admits_and_executes_valid_order() {
    let node = SnubberNode::builder("cnc_mill").build();
    let laser_killed = Arc::new(AtomicBool::new(false));
    let rb = Arc::clone(&laser_killed);

    struct CNCJob {
        depth_mm: f64,
        speed_rpm: u32,
    }

    let valid_job = CNCJob {
        depth_mm: 5.0,
        speed_rpm: 12000,
    };

    let cut_counter = Arc::new(AtomicU64::new(0));
    let cnt = Arc::clone(&cut_counter);

    let res: Result<f64, SnubberError<&'static str>> = node.scoped_checked(
        &valid_job,
        |job| {
            snub_check!(
                job.depth_mm > 0.0 && job.depth_mm <= 25.0,
                "depth out of range"
            );
            snub_check!(job.speed_rpm >= 1000, "spindle speed too low");
            Ok(())
        },
        move || {
            rb.store(true, Ordering::SeqCst);
        },
        move |job| {
            cnt.fetch_add(1, Ordering::SeqCst);
            Ok(job.depth_mm * 2.0)
        },
    );

    assert_eq!(res.unwrap(), 10.0);
    assert_eq!(cut_counter.load(Ordering::SeqCst), 1);
    assert!(!laser_killed.load(Ordering::SeqCst));

    let stats = node.stats();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.total_started, 1);
}
