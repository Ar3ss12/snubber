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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use core::task::{Context, Poll, Waker};
use std::sync::{Arc, Condvar, Mutex, RwLock};

/// Directives sent by the caller/observer to the executing worker.
#[must_use = "IGNORING DIRECTIVES INVITES DEADLOCK: match Proceed/Pause/Cancel/SkipStage/Preempted — never drop the verdict sight unseen"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Directive {
    /// Continue execution.
    Proceed,
    /// Pause execution until resumed.
    Pause,
    /// Cancel execution immediately with a static reason code.
    Cancel(&'static str),
    /// Skip current iteration or stage and jump ahead.
    SkipStage,
    /// Phase 1 preemption: a `Priority::Critical` admission requested this
    /// `Background` task to yield its slot. Cooperative (R5 — no
    /// external thread termination): the worker observes this on its next
    /// `check_directive()` / `wait_for_resume_async()` and settles
    /// itself (`abort("preempted")` / `commit` the partial + drop).
    /// Workers must poll cooperative checkpoints periodically; the node does not forcibly preempt.
    Preempted(&'static str),
}

const DIR_PROCEED: u8 = 0;
const DIR_PAUSE: u8 = 1;
const DIR_CANCEL: u8 = 2;
const DIR_SKIP_STAGE: u8 = 3;
const DIR_PREEMPTED: u8 = 4;

// COMPILE-TIME GATES:
// Directives must map to pairwise distinct atomic discriminants (0..=4).
const _: () = {
    assert!(DIR_PROCEED != DIR_PAUSE);
    assert!(DIR_PROCEED != DIR_CANCEL);
    assert!(DIR_PROCEED != DIR_SKIP_STAGE);
    assert!(DIR_PROCEED != DIR_PREEMPTED);
    assert!(DIR_PAUSE != DIR_CANCEL);
    assert!(DIR_PAUSE != DIR_SKIP_STAGE);
    assert!(DIR_PAUSE != DIR_PREEMPTED);
    assert!(DIR_CANCEL != DIR_SKIP_STAGE);
    assert!(DIR_CANCEL != DIR_PREEMPTED);
    assert!(DIR_SKIP_STAGE != DIR_PREEMPTED);
};

struct IntercomInner<S, D> {
    directive: AtomicU8,
    cancel_reason: Mutex<Option<&'static str>>,
    state: RwLock<S>,
    draft: Mutex<Option<D>>,
    conflated: AtomicU64,
    condvar: Condvar,
    wakers: Mutex<Vec<ParkedResumeWaker>>,
}

/// A parked `ResumeFuture` waiter: the waker plus an identity flag minted
/// per future. Identity (`Arc::ptr_eq`) — not `will_wake` — drives dedup
/// and `Drop` removal: two futures polled by one task share a waker, so a
/// `will_wake` scan would cross-match and a sibling's cancellation would
/// erase this entry (same class as the node's waiter queue, node.rs).
struct ParkedResumeWaker {
    waker: Waker,
    identity: Arc<AtomicBool>,
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

/// The caller/observer's control handle for inspecting and steering the task.
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
            conflated: AtomicU64::new(0),
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
    ///
    /// Poison-recovering (same discipline as `TaskHandle::inspect`): a panicked
    /// state closure must not freeze all future state publishing — the
    /// poison is absorbed and the closure still runs on the recovered guard.
    pub fn update_state<F: FnOnce(&mut S)>(&self, f: F) {
        let mut lock = match self.inner.state.write() {
            Ok(l) => l,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut lock);
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
                    .map(|r| *r)
                    .unwrap_or_else(|poisoned| *poisoned.into_inner())
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
                    .map(|r| *r)
                    .unwrap_or_else(|poisoned| *poisoned.into_inner())
                    .unwrap_or("preempted");
                Directive::Preempted(reason)
            }
            _ => Directive::Proceed,
        }
    }

    /// Synchronously wait until caller resumes or cancels. Blocks current OS thread using `Condvar`.
    /// Zero polling: the worker sleeps in the OS kernel at 0% CPU until
    /// `TaskHandle::resume` / `send_directive(Proceed|Cancel|Preempted)`
    /// issues `condvar.notify_all()`. Spurious wakeups re-check the
    /// directive word, so exit latency is one context switch, not a quantum.
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
            identity: None,
        }
    }

    /// Blocking pause gate: sleep on the `Condvar` while a `Pause`
    /// directive is armed, return the directive that released the worker.
    ///
    /// Canonical worker-loop shape (no `thread::sleep` polling):
    /// ```rust,no_run
    /// use snubber::{TaskConduit, Directive};
    /// let (conduit, handle): (TaskConduit<u8>, _) = TaskConduit::pair(0);
    /// loop {
    ///     match conduit.wait_while_paused() {
    ///         Directive::Proceed | Directive::SkipStage => { /* one unit */ }
    ///         Directive::Pause => continue, // re-armed before wake; re-park
    ///         Directive::Cancel(r) | Directive::Preempted(r) => break, // r: &'static str
    ///         // `Directive` is #[non_exhaustive]: unknown steering stops the unit.
    ///         _ => break,
    ///     }
    ///     let _ = &handle;
    ///     break;
    /// }
    /// ```
    pub fn wait_while_paused(&self) -> Directive {
        // Fast path: no pause armed.
        match self.check_directive() {
            Directive::Pause => {}
            other => return other,
        }
        // Slow path: kernel sleep; Cancel/Preempted surface as Err(reason).
        match self.wait_for_resume() {
            Ok(()) => self.check_directive(),
            Err(reason) => match self.check_directive() {
                Directive::Preempted(_) => Directive::Preempted(reason),
                _ => Directive::Cancel(reason),
            },
        }
    }

    /// Publish an intermediate draft artifact onto the shared table.
    ///
    /// The table is a **single slot**: a newer draft overwrites an
    /// unsampled older one. Returns `true` when that happened, so a
    /// producer can report coalescing without a second counter read.
    pub fn publish_draft(&self, draft: D) -> bool {
        let mut lock = match self.inner.draft.lock() {
            Ok(l) => l,
            // A poisoned draft table still holds a valid slot: recover it
            // instead of silently dropping the publish.
            Err(poisoned) => poisoned.into_inner(),
        };
        let replaced = lock.is_some();
        if replaced {
            self.inner.conflated.fetch_add(1, Ordering::Relaxed);
        }
        *lock = Some(draft);
        replaced
    }

    /// Take the draft artifact from the shared table (if any).
    pub fn take_draft(&self) -> Option<D> {
        match self.inner.draft.lock() {
            Ok(mut d) => d.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    /// Drafts overwritten before the caller sampled them.
    ///
    /// `published - taken == conflated` holds for single-slot traffic:
    /// every publish that did not land in an empty slot coalesced an
    /// unsampled draft. Monotonic, relaxed, never blocks.
    #[must_use]
    pub fn conflated_drafts(&self) -> u64 {
        self.inner.conflated.load(Ordering::Relaxed)
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
                // A poisoned reason cell still holds a valid slot. `.ok()`
                // silently dropped the caller's reason and `wait_for_resume`
                // fell back to the generic "cancelled" — recover, never
                // discard the directive payload.
                let mut reason_lock = match self.inner.cancel_reason.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *reason_lock = Some(reason);
                self.inner.directive.store(DIR_CANCEL, Ordering::Release);
                // Notify while already holding `cancel_reason` (see
                // `notify_sync`): same lost-wakeup discipline, no re-lock.
                self.inner.condvar.notify_all();
                drop(reason_lock);
                self.wake_async();
            }
            Directive::SkipStage => {
                self.inner
                    .directive
                    .store(DIR_SKIP_STAGE, Ordering::Release);
                Self::notify_sync(&self.inner);
                self.wake_async();
            }
            Directive::Preempted(reason) => {
                let mut reason_lock = match self.inner.cancel_reason.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *reason_lock = Some(reason);
                self.inner.directive.store(DIR_PREEMPTED, Ordering::Release);
                self.inner.condvar.notify_all();
                drop(reason_lock);
                self.wake_async();
            }
        }
    }

    /// Resume a paused worker (wakes sync condvar and async wakers).
    pub fn resume(&self) {
        self.inner.directive.store(DIR_PROCEED, Ordering::Release);
        Self::notify_sync(&self.inner);
        self.wake_async();
    }

    /// Wake the sync condvar while HOLDING the `cancel_reason` mutex.
    ///
    /// The sync waiter (`wait_for_resume`) holds that mutex across its
    /// directive-check → `condvar.wait` window. Notifying under the same
    /// lock makes "store directive → notify" and "check directive → park"
    /// mutually exclusive; notifying without it lets the notification land
    /// in the gap before the waiter actually parks — the classic lost
    /// wakeup, leaving the worker asleep forever on a resumed conduit.
    fn notify_sync(inner: &IntercomInner<S, D>) {
        // Recover a poisoned mutex: `.ok()` would drop the guard and issue
        // the notify UNLOCKED, re-opening the exact lost-wakeup window this
        // function exists to close.
        let _guard = match inner.cancel_reason.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.condvar.notify_all();
    }

    /// Take the draft artifact from the shared table.
    pub fn take_draft(&self) -> Option<D> {
        // Parity with `TaskConduit::take_draft`: a poisoned table still
        // holds a valid slot — `.ok()` would make the draft vanish.
        match self.inner.draft.lock() {
            Ok(mut d) => d.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    /// Drafts overwritten before the caller sampled them (mirror of
    /// [`TaskConduit::conflated_drafts`]).
    #[must_use]
    pub fn conflated_drafts(&self) -> u64 {
        self.inner.conflated.load(Ordering::Relaxed)
    }

    fn wake_async(&self) {
        // Wake OUTSIDE the lock: `wake()` runs executor code, and a panic
        // under the wakers mutex would poison it and strand every later
        // waiter. `mem::take` hands over the existing Vec without
        // allocating under the lock.
        let parked = match self.inner.wakers.lock() {
            Ok(mut wakers) => core::mem::take(&mut *wakers),
            Err(poisoned) => core::mem::take(&mut *poisoned.into_inner()),
        };
        for p in parked {
            p.waker.wake();
        }
    }
}

/// Future for asynchronous pause/resume waiting.
///
/// Zero-bound like the intercom itself: polling never duplicates `S`.
///
/// Cancellation-safe: the waker pushed while parked is remembered in the
/// future itself and removed from the queue on `Drop`, so a future dropped
/// mid-pause (e.g. a timeout branch in `select!`) cannot leave a dead waker
/// behind — each later `resume` wakes only live waiters.
pub struct ResumeFuture<S, D> {
    inner: Arc<IntercomInner<S, D>>,
    identity: Option<Arc<AtomicBool>>,
}

impl<S, D> Drop for ResumeFuture<S, D> {
    fn drop(&mut self) {
        if let Some(flag) = self.identity.take() {
            // Identity-matched removal: only THIS future's entry is
            // retired, never a sibling's (a shared waker would make a
            // `will_wake`-based retain erase both).
            match self.inner.wakers.lock() {
                Ok(mut wakers) => wakers.retain(|q| !Arc::ptr_eq(&q.identity, &flag)),
                Err(poisoned) => poisoned
                    .into_inner()
                    .retain(|q| !Arc::ptr_eq(&q.identity, &flag)),
            }
        }
    }
}

impl<S, D> Future for ResumeFuture<S, D> {
    type Output = Result<(), &'static str>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Unpin: the only field is an Arc plus an Option<Waker>.
        let this = self.get_mut();
        // P0 fix: read the directive WHILE holding the wakers lock.
        //
        // wake_async() also acquires the wakers lock before draining.
        // This makes "write directive → drain wakers" and
        // "read directive → push waker" mutually exclusive:
        //
        //   Case A — controller wins the lock first:
        //     writes PROCEED → drains (empty list) → releases lock.
        //     poller takes lock → sees PROCEED → Ready(Ok(())), no waker pushed.
        //
        //   Case B — poller wins the lock first:
        //     reads PAUSE → pushes waker → returns Pending → releases lock.
        //     controller takes lock → drains → wakes the task.
        //
        // The previous "register-then-read" approach had a gap between
        // release of wakers lock and the directive load, leaving Case A
        // open: controller could drain (empty list) before poller pushed,
        // and poller would park forever.
        // Clone before the lock: `Waker::clone` runs foreign vtable code
        // and may allocate; no executor code under the wakers mutex.
        let waker = cx.waker().clone();
        let mut wakers = match this.inner.wakers.lock() {
            Ok(w) => w,
            Err(poisoned) => poisoned.into_inner(),
        };
        let dir = this.inner.directive.load(Ordering::Acquire);
        match dir {
            DIR_PAUSE => {
                // Identity dedup: repeated polls of THIS future update its
                // own entry (matched by the minted flag), never a sibling's.
                if let Some(flag) = this.identity.as_ref() {
                    if let Some(slot) = wakers.iter_mut().find(|q| Arc::ptr_eq(&q.identity, flag)) {
                        // Drop the replaced waker after the lock: its Drop
                        // runs executor code and may take executor locks.
                        let old = core::mem::replace(&mut slot.waker, waker);
                        drop(wakers);
                        drop(old);
                        return Poll::Pending;
                    }
                }
                let flag = Arc::new(AtomicBool::new(false));
                this.identity = Some(Arc::clone(&flag));
                wakers.push(ParkedResumeWaker {
                    waker,
                    identity: flag,
                });
                // Remember the pushed entry so Drop can retire it.
                Poll::Pending
            }
            DIR_CANCEL | DIR_PREEMPTED => {
                // The entry is about to resolve; nothing is parked anymore.
                let reason = {
                    let r = match this.inner.cancel_reason.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    (*r).unwrap_or("cancelled")
                };
                Poll::Ready(Err(reason))
            }
            _ => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod poison_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn poison<T>(lock: &Mutex<T>) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = lock.lock().expect("fresh lock");
            panic!("deliberate poison");
        }));
        assert!(lock.is_poisoned(), "the panic must poison the mutex");
    }

    #[test]
    fn poisoned_cancel_reason_keeps_the_directive_payload() {
        let (conduit, handle): (TaskConduit<u8>, TaskHandle<u8>) = TaskConduit::pair(0);
        poison(&handle.inner.cancel_reason);
        handle.send_directive(Directive::Cancel("stage 3 aborted"));
        match conduit.check_directive() {
            Directive::Cancel(reason) => assert_eq!(reason, "stage 3 aborted"),
            other => panic!("expected Cancel, got {other:?}"),
        }
        let _ = conduit.check_directive();
        poison(&handle.inner.cancel_reason);
        handle.send_directive(Directive::Preempted("critical lane"));
        match conduit.check_directive() {
            Directive::Preempted(reason) => assert_eq!(reason, "critical lane"),
            other => panic!("expected Preempted, got {other:?}"),
        }
    }

    #[test]
    fn poisoned_draft_table_still_yields_the_draft() {
        let (conduit, handle): (TaskConduit<u8, u32>, TaskHandle<u8, u32>) = TaskConduit::pair(0);
        assert!(!conduit.publish_draft(41));
        poison(&handle.inner.draft);
        assert_eq!(
            handle.take_draft(),
            Some(41),
            "poison must not eat the draft"
        );
        assert_eq!(handle.take_draft(), None);
    }
}
