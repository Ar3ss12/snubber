//! Bidirectional Intercom / Task Steering Protocol between Caller and Callee.
//!
//! Separates execution into two roles:
//! * **[`TaskConduit`] (Worker / Callee):** operates inside the running function,
//!   publishes live state updates, probes directives, and can sleep/resume or hand over drafts.
//! * **[`TaskHandle`] (Caller / Observer / Controller):** inspects state snapshots on demand,
//!   issues runtime directives (`Pause`, `Cancel`, `SkipStage`), and reads drafts.
//!
//! Supports both **synchronous** (`Condvar` sleep) and **asynchronous** (`core::task::Waker` parking)
//! workflows with zero external dependencies and `#![forbid(unsafe_code)]`.

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::{Context, Poll, Waker};
use std::sync::{Arc, Condvar, Mutex, RwLock};

/// Directives sent by the caller/observer to the executing worker.
#[must_use = "IGNORING DIRECTIVES INVITES DEADLOCK: match Proceed/Pause/Cancel/SkipStage/Preempted — never drop the verdict sight unseen"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directive {
    /// Normal flight: proceed execution.
    Proceed,
    /// Hold engine: pause execution until resumed.
    Pause,
    /// Abort mission: cancel immediately with a static reason code.
    Cancel(&'static str),
    /// Skip current iteration or stage and jump ahead.
    SkipStage,
    /// Phase 1 preemption: a `Priority::Critical` admission asked this
    /// `Background` task to yield its slot NOW. Cooperative (R5 — no
    /// killer threads): the worker observes this on its next
    /// `check_directive()` / `wait_for_resume_async()` and settles
    /// itself (`abort("preempted")` / `commit` the partial + drop).
    /// Who never checkpoints is its own enemy — the node never forces.
    Preempted(&'static str),
}

const DIR_PROCEED: u8 = 0;
const DIR_PAUSE: u8 = 1;
const DIR_CANCEL: u8 = 2;
const DIR_SKIP_STAGE: u8 = 3;
const DIR_PREEMPTED: u8 = 4;

struct IntercomInner<S, D> {
    directive: AtomicU8,
    cancel_reason: Mutex<Option<&'static str>>,
    state: RwLock<S>,
    draft: Mutex<Option<D>>,
    condvar: Condvar,
    wakers: Mutex<Vec<Waker>>,
}

/// The worker's task conduit gateway inside the executing task.
///
/// `S`/`D` carry NO `Clone` bound here on purpose: sharing the intercom
/// (cloning the inner `Arc`) must work for any payload, including
/// non-`Clone` drafts. Methods that actually duplicate the payload
/// (`TaskHandle::inspect`) declare `S: Clone` locally on themselves.
pub struct TaskConduit<S, D = ()> {
    inner: Arc<IntercomInner<S, D>>,
}

/// The caller/observer's control console for inspecting and steering the task.
///
/// Same zero-bound contract as the conduit above.
pub struct TaskHandle<S, D = ()> {
    inner: Arc<IntercomInner<S, D>>,
}

impl<S, D> TaskConduit<S, D> {
    /// Create a connected pair: conduit for the worker and handle for the caller.
    pub fn pair(initial_state: S) -> (Self, TaskHandle<S, D>) {
        let inner = Arc::new(IntercomInner {
            directive: AtomicU8::new(DIR_PROCEED),
            cancel_reason: Mutex::new(None),
            state: RwLock::new(initial_state),
            draft: Mutex::new(None),
            condvar: Condvar::new(),
            wakers: Mutex::new(Vec::new()),
        });
        (
            TaskConduit {
                inner: inner.clone(),
            },
            TaskHandle { inner },
        )
    }

    /// Update the live telemetry state via a mutating closure.
    pub fn update_state<F: FnOnce(&mut S)>(&self, f: F) {
        if let Ok(mut lock) = self.inner.state.write() {
            f(&mut lock);
        }
    }

    /// Fast, non-blocking check for pending directives from the caller.
    pub fn check_directive(&self) -> Directive {
        match self.inner.directive.load(Ordering::Acquire) {
            DIR_PAUSE => Directive::Pause,
            DIR_CANCEL => {
                let reason = self
                    .inner
                    .cancel_reason
                    .lock()
                    .ok()
                    .and_then(|r| *r)
                    .unwrap_or("cancelled");
                Directive::Cancel(reason)
            }
            DIR_SKIP_STAGE => {
                // Auto-acknowledge skip directive back to proceed
                self.inner.directive.store(DIR_PROCEED, Ordering::Release);
                Directive::SkipStage
            }
            DIR_PREEMPTED => {
                let reason = self
                    .inner
                    .cancel_reason
                    .lock()
                    .ok()
                    .and_then(|r| *r)
                    .unwrap_or("preempted");
                Directive::Preempted(reason)
            }
            _ => Directive::Proceed,
        }
    }

    /// Synchronously wait until caller resumes or cancels. Blocks current OS thread using `Condvar`.
    pub fn wait_for_resume(&self) -> Result<(), &'static str> {
        let mut reason_guard = self
            .inner
            .cancel_reason
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        while self.inner.directive.load(Ordering::Acquire) == DIR_PAUSE {
            reason_guard = self
                .inner
                .condvar
                .wait(reason_guard)
                .unwrap_or_else(|p| p.into_inner());
        }
        match self.inner.directive.load(Ordering::Acquire) {
            DIR_CANCEL | DIR_PREEMPTED => Err(reason_guard.unwrap_or("cancelled")),
            _ => Ok(()),
        }
    }

    /// Asynchronously wait until caller resumes or cancels. Parks `Waker` at 0% CPU.
    pub fn wait_for_resume_async(&self) -> ResumeFuture<S, D> {
        ResumeFuture {
            inner: self.inner.clone(),
        }
    }

    /// Publish an intermediate draft artifact onto the shared table.
    pub fn publish_draft(&self, draft: D) {
        if let Ok(mut lock) = self.inner.draft.lock() {
            *lock = Some(draft);
        }
    }

    /// Take the draft artifact from the shared table (if any).
    pub fn take_draft(&self) -> Option<D> {
        self.inner.draft.lock().ok().and_then(|mut d| d.take())
    }
}

impl<S, D> Clone for TaskConduit<S, D> {
    /// Shared-ownership clone: bumps the `Arc` refcount only. No `Clone`
    /// bound on `S`/`D` — deliberately NOT `#[derive(Clone)]`, which would
    /// demand `S: Clone, D: Clone` even though cloning the `Arc` never
    /// touches the payload. A non-`Clone` directive type must never block
    /// handing control to another thread.
    #[inline]
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S, D> Clone for TaskHandle<S, D> {
    /// Same as above: the handle is a second view of the same intercom.
    #[inline]
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S, D> TaskHandle<S, D> {
    /// Read an instant snapshot of the worker's live state.
    pub fn inspect(&self) -> S
    where
        S: Clone,
    {
        self.inner
            .state
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// Send a directive to the worker.
    pub fn send_directive(&self, directive: Directive) {
        match directive {
            Directive::Proceed => self.resume(),
            Directive::Pause => {
                self.inner.directive.store(DIR_PAUSE, Ordering::Release);
            }
            Directive::Cancel(reason) => {
                if let Ok(mut lock) = self.inner.cancel_reason.lock() {
                    *lock = Some(reason);
                }
                self.inner.directive.store(DIR_CANCEL, Ordering::Release);
                self.inner.condvar.notify_all();
                self.wake_async();
            }
            Directive::SkipStage => {
                self.inner
                    .directive
                    .store(DIR_SKIP_STAGE, Ordering::Release);
                self.inner.condvar.notify_all();
                self.wake_async();
            }
            Directive::Preempted(reason) => {
                if let Ok(mut lock) = self.inner.cancel_reason.lock() {
                    *lock = Some(reason);
                }
                self.inner.directive.store(DIR_PREEMPTED, Ordering::Release);
                self.inner.condvar.notify_all();
                self.wake_async();
            }
        }
    }

    /// Resume a paused worker (wakes sync condvar and async wakers).
    pub fn resume(&self) {
        self.inner.directive.store(DIR_PROCEED, Ordering::Release);
        self.inner.condvar.notify_all();
        self.wake_async();
    }

    /// Take the draft artifact from the shared table.
    pub fn take_draft(&self) -> Option<D> {
        self.inner.draft.lock().ok().and_then(|mut d| d.take())
    }

    fn wake_async(&self) {
        if let Ok(mut wakers) = self.inner.wakers.lock() {
            for w in wakers.drain(..) {
                w.wake();
            }
        }
    }
}

/// Future for asynchronous pause/resume waiting.
///
/// Zero-bound like the intercom itself: polling never duplicates `S`.
pub struct ResumeFuture<S, D> {
    inner: Arc<IntercomInner<S, D>>,
}

impl<S, D> Future for ResumeFuture<S, D> {
    type Output = Result<(), &'static str>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let dir = self.inner.directive.load(Ordering::Acquire);
        if dir == DIR_PAUSE {
            if let Ok(mut wakers) = self.inner.wakers.lock() {
                wakers.push(cx.waker().clone());
            }
            Poll::Pending
        } else if dir == DIR_CANCEL || dir == DIR_PREEMPTED {
            let reason = self
                .inner
                .cancel_reason
                .lock()
                .ok()
                .and_then(|r| *r)
                .unwrap_or("cancelled");
            Poll::Ready(Err(reason))
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
