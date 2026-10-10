//! Integration tests for Intercom / Cockpit Protocol (TaskConduit & TaskHandle).

use core::future::Future;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::time::Duration;
use snubber::{Directive, TaskConduit};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PilotState {
    stage: &'static str,
    progress: u32,
    items: usize,
}

#[test]
fn sync_intercom_live_inspection_and_directives() {
    let (conduit, handle) = TaskConduit::<PilotState>::pair(PilotState::default());

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
                Directive::Cancel(_) => return Err("cancelled"),
                Directive::SkipStage => break,
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        conduit.update_state(|s| s.stage = "Completed");
        finished_clone.store(true, Ordering::SeqCst);
        Ok(())
    });

    // Wait until worker updates progress
    std::thread::sleep(Duration::from_millis(5));
    let snap = handle.inspect();
    assert!(snap.progress > 0);
    assert_eq!(snap.stage, "Started");

    // Pause worker
    handle.send_directive(Directive::Pause);
    std::thread::sleep(Duration::from_millis(10));
    let paused_progress = handle.inspect().progress;

    // Verify worker didn't move while paused
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(handle.inspect().progress, paused_progress);

    // Resume worker
    handle.resume();

    // Send skip stage to fast-forward
    std::thread::sleep(Duration::from_millis(5));
    handle.send_directive(Directive::SkipStage);

    let _ = worker.join().unwrap();
    assert!(finished.load(Ordering::SeqCst));
    assert_eq!(handle.inspect().stage, "Completed");
}

#[test]
fn sync_intercom_cancel_directive_aborts_immediately() {
    let (conduit, handle) = TaskConduit::<PilotState>::pair(PilotState::default());

    let worker = std::thread::spawn(move || -> Result<(), &'static str> {
        conduit.update_state(|s| s.stage = "Running");
        loop {
            match conduit.check_directive() {
                Directive::Cancel(reason) => return Err(reason),
                _ => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    });

    std::thread::sleep(Duration::from_millis(5));
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

    let (conduit, handle) = TaskConduit::<PilotState, MatrixDraft>::pair(PilotState::default());

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
    let (conduit, handle) = TaskConduit::<PilotState>::pair(PilotState::default());

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
