//! Integration tests for Task Steering Protocol (TaskConduit & TaskHandle).

use core::future::Future;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::time::Duration;
use snubber::{Directive, TaskConduit};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorkerState {
    stage: &'static str,
    progress: u32,
    items: usize,
}

#[test]
fn sync_intercom_live_inspection_and_directives() {
    let (conduit, handle) = TaskConduit::<WorkerState>::pair(WorkerState::default());

    let finished = Arc::new(AtomicBool::new(false));
    let finished_clone = finished.clone();

    let worker = std::thread::spawn(move || {
        conduit.update_state(|s| {
            s.stage = "Started";
            s.progress = 10;
        });

        // Loop simulating work
        for i in 1..=100 {
            conduit.update_state(|s| {
                s.progress = i;
            });

            match conduit.check_directive() {
                Directive::Proceed => {}
                Directive::Pause => {
                    let _ = conduit.wait_for_resume();
                }
                Directive::Cancel(_) | Directive::Preempted(_) => return Err("cancelled"),
                Directive::SkipStage => break,
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        conduit.update_state(|s| s.stage = "Completed");
        finished_clone.store(true, Ordering::SeqCst);
        Ok(())
    });

    // Wait until worker updates progress (up to 2 seconds timeout)
    let start = std::time::Instant::now();
    while handle.inspect().progress == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timeout waiting for progress"
        );
        std::thread::yield_now();
    }
    let snap = handle.inspect();
    assert!(snap.progress > 0);
    assert_eq!(snap.stage, "Started");

    // Pause worker
    handle.send_directive(Directive::Pause);

    // Give worker time to reach Pause checkpoint
    let pause_start = std::time::Instant::now();
    let mut last_progress = handle.inspect().progress;
    let mut stable_cycles = 0;
    while pause_start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(2));
        let curr = handle.inspect().progress;
        if curr == last_progress {
            stable_cycles += 1;
            if stable_cycles >= 5 {
                break;
            }
        } else {
            stable_cycles = 0;
            last_progress = curr;
        }
    }
    let paused_progress = handle.inspect().progress;

    // Verify worker didn't move while paused
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(handle.inspect().progress, paused_progress);

    // Resume worker
    handle.resume();

    // Send skip stage to fast-forward
    let resume_start = std::time::Instant::now();
    while handle.inspect().progress == paused_progress {
        if resume_start.elapsed() > Duration::from_secs(2) {
            break;
        }
        std::thread::yield_now();
    }
    handle.send_directive(Directive::SkipStage);

    let _ = worker.join().unwrap();
    assert!(finished.load(Ordering::SeqCst));
    assert_eq!(handle.inspect().stage, "Completed");
}

#[test]
fn sync_intercom_cancel_directive_aborts_immediately() {
    let (conduit, handle) = TaskConduit::<WorkerState>::pair(WorkerState::default());

    let worker = std::thread::spawn(move || -> Result<(), &'static str> {
        conduit.update_state(|s| s.stage = "Running");
        loop {
            match conduit.check_directive() {
                Directive::Cancel(reason) => return Err(reason),
                _ => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    });

    let start = std::time::Instant::now();
    while handle.inspect().stage != "Running" {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timeout waiting for worker running"
        );
        std::thread::yield_now();
    }
    assert_eq!(handle.inspect().stage, "Running");

    handle.send_directive(Directive::Cancel("mission_aborted"));
    let res = worker.join().unwrap();
    assert_eq!(res, Err("mission_aborted"));
}

#[test]
fn shared_table_draft_handover_between_stages() {
    #[derive(Debug, PartialEq, Eq, Clone)]
    struct MatrixDraft {
        rows: usize,
        cols: usize,
        signature: u64,
    }

    let (conduit, handle) = TaskConduit::<WorkerState, MatrixDraft>::pair(WorkerState::default());

    // Stage 1 (Generator): builds draft and places on shared table
    conduit.update_state(|s| s.stage = "GeneratingMatrix");
    let draft = MatrixDraft {
        rows: 4,
        cols: 4,
        signature: 0xDEADBEEF,
    };
    conduit.publish_draft(draft.clone());

    // Observer / Auditor inspects from outside:
    assert_eq!(handle.inspect().stage, "GeneratingMatrix");

    // Stage 2 (Consumer/Auditor): takes draft from shared table
    let taken = conduit.take_draft();
    assert_eq!(taken, Some(draft));
    assert_eq!(conduit.take_draft(), None); // Table cleared
}

#[test]
fn async_intercom_pause_and_resume_waking() {
    let (conduit, handle) = TaskConduit::<WorkerState>::pair(WorkerState::default());

    let mut fut = Box::pin(conduit.wait_for_resume_async());
    let waker = fake_waker();
    let mut cx = Context::from_waker(&waker);

    // Initially proceed -> returns Ready(Ok(()))
    assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(())));

    // Set Pause
    handle.send_directive(Directive::Pause);
    let mut fut_paused = Box::pin(conduit.wait_for_resume_async());
    assert_eq!(fut_paused.as_mut().poll(&mut cx), Poll::Pending);

    // Resume -> should return Ready(Ok(()))
    handle.resume();
    assert_eq!(fut_paused.as_mut().poll(&mut cx), Poll::Ready(Ok(())));

    // Cancel while paused -> should return Ready(Err(reason))
    handle.send_directive(Directive::Pause);
    let mut fut_cancel = Box::pin(conduit.wait_for_resume_async());
    assert_eq!(fut_cancel.as_mut().poll(&mut cx), Poll::Pending);

    handle.send_directive(Directive::Cancel("remote_kill"));
    assert_eq!(
        fut_cancel.as_mut().poll(&mut cx),
        Poll::Ready(Err("remote_kill"))
    );
}

fn fake_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

/// Zero-Bound Clone Invariant: hand-written `Clone` on the intercom pair must
/// carry NO bounds. `#[derive(Clone)]` would demand `S: Clone, D: Clone`
/// even though cloning the inner `Arc` never touches the payload — a
/// non-`Clone` draft/directive type would then forbid handing control to
/// another thread. This test pins the zero-bound contract at compile time:

///
/// ```text
/// struct NoClone; // deliberately NOT Clone
/// let (c, h) = TaskConduit::<WorkerState, NoClone>::pair(...);
/// let _ = c.clone(); // must compile
/// ```
#[test]
fn intercom_clone_needs_no_payload_bounds() {
    struct NoCloneDraft {
        _opaque: core::cell::Cell<u64>,
    }
    // NOTE: `NoCloneDraft` deliberately does NOT implement `Clone`. If the
    // `Clone` impl on the intercom ever gains `D: Clone` (e.g. via derive),
    // the two `.clone()` lines below stop compiling and this test goes red.
    let (conduit, handle) = TaskConduit::<WorkerState, NoCloneDraft>::pair(WorkerState::default());
    // Clone BEFORE any draft exists — exercises the Arc bump only.
    let _conduit2 = conduit.clone();
    let handle2 = handle.clone();
    // Both views still observe the same shared table.
    conduit.update_state(|s| s.progress = 7);
    assert_eq!(handle.inspect().progress, 7);
    assert_eq!(handle2.inspect().progress, 7);
    // Publish a non-Clone draft through one view, take it through the clone.
    conduit.publish_draft(NoCloneDraft {
        _opaque: core::cell::Cell::new(0xC10C),
    });
    assert!(handle2.take_draft().is_some());
    assert!(handle.take_draft().is_none());
}
