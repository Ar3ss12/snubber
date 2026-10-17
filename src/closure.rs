//! Closure-capturing self-healing commands behind `alloc`.
//!
//! Core `FaultAction::Custom(fn())` stays `Copy` and heap-free for firmware.
//! When the binary CAN allocate, this module adds the missing half:
//! `ClosurePolicy` — a `Box<dyn Fn()>` command with captured context
//! (driver handle, resubscribe closure, backoff state), executed OUTSIDE
//! the node lock with the same contract as every hook: fast,
//! non-panicking, lock-free on real-time paths.
//!
//! Since 0.3 `alloc` rides with `default` (`std = ["alloc"]`): desktop and
//! server binaries get capturing commands out of the box; `no_std`
//! firmware opts out with `default-features = false`.
//!
//! Also home of [`ErasedGuard`]: the type-erased rollback slot. Generic
//! `SnubberGuard<F>` / `TrackedGuard<F>` cannot live in a struct field
//! (every closure is a different anonymous type — the `HOLD_SLOTS`
//! registry dance from the field integration proved it). `ErasedGuard`
//! boxes the rollback once at arming time, then behaves like any guard:
//! `commit()` disarms, `Drop` rolls back, and it fits in
//! `struct App { hold: Option<ErasedGuard> }` with zero user registries.

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
use alloc::boxed::Box;

/// Boxed self-healing command with captured context.
///
/// ```rust
/// use snubber::ClosurePolicy;
/// let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
/// let c = attempts.clone();
/// let policy = ClosurePolicy::new(move || {
///     c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
/// });
/// policy.run();
/// assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
/// ```
#[cfg(feature = "alloc")]
pub struct ClosurePolicy {
    cmd: Box<dyn Fn() + Send + Sync>,
}

#[cfg(feature = "alloc")]
impl ClosurePolicy {
    /// Box a capturing closure as a node-executed command.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self { cmd: Box::new(f) }
    }

    /// Execute OUTSIDE the node lock (called by `SnubberNode::emit`).
    #[inline]
    pub fn run(&self) {
        (self.cmd)();
    }
}

#[cfg(feature = "alloc")]
impl core::fmt::Debug for ClosurePolicy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ClosurePolicy(..)")
    }
}

/// Type-erased rollback slot: `Box<dyn FnOnce()>` behind a guard face.
///
/// Arming erases the closure type ONCE (`Box::new`), so the guard itself
/// is a concrete, nameable type that fits struct fields, `Option`, `Vec`,
/// statics — no per-user registries, no task-id plumbing:
///
/// ```rust,no_run
/// use snubber::{ErasedGuard, SnubberNode};
/// struct App {
///     hold: Option<ErasedGuard>, // ← was impossible with TrackedGuard<F>
/// }
/// let node = SnubberNode::builder("drv").build();
/// let mut app = App { hold: None };
/// app.hold = Some(node.enter_erased(1, 7, || release_all_buttons()).unwrap());
/// // … thread death here still releases the buttons via Drop …
/// app.hold.take().map(|g| g.commit()); // clean stop
/// # fn release_all_buttons() {}
/// ```
///
/// Semantics mirror [`crate::SnubberGuard`]: `commit()` consumes and
/// disarms, `abort(reason)` rolls back NOW, `Drop` without settle rolls
/// back. Requires `alloc` (the box); `no_std` firmware keeps the generic
/// zero-cost guards.
#[cfg(feature = "alloc")]
pub struct ErasedGuard {
    rollback: Option<Box<dyn FnOnce() + Send + 'static>>,
    on_commit: Option<Box<dyn FnOnce() + Send + 'static>>,
}

#[cfg(feature = "alloc")]
impl ErasedGuard {
    /// Box a rollback closure into a nameable guard.
    pub fn new<F>(rollback: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            rollback: Some(Box::new(rollback)),
            on_commit: None,
        }
    }

    /// Box a rollback closure and an optional clean-commit hook.
    pub fn with_commit_hook<R, C>(rollback: R, on_commit: C) -> Self
    where
        R: FnOnce() + Send + 'static,
        C: FnOnce() + Send + 'static,
    {
        Self {
            rollback: Some(Box::new(rollback)),
            on_commit: Some(Box::new(on_commit)),
        }
    }

    /// Success path: disarm the rollback, invoke commit hook if registered, no-op on drop.
    ///
    /// Same `SeqCst` hygiene as [`SnubberGuard`](crate::SnubberGuard): fence
    /// before the disarm; foreign sharing still needs `Release`/`Acquire`.
    #[inline]
    pub fn commit(mut self) {
        let _ = self.rollback.take();
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        if let Some(on_commit) = self.on_commit.take() {
            #[cfg(feature = "std")]
            {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(on_commit));
            }
            #[cfg(not(feature = "std"))]
            {
                on_commit();
            }
        }
        core::mem::forget(self);
    }

    /// Planned failure path: rollback NOW, then forget.
    ///
    /// `SeqCst` fence before the body (same honesty as the core guard).
    #[inline]
    pub fn abort(mut self, _reason: &'static str) {
        let _ = self.on_commit.take();
        if let Some(rollback) = self.rollback.take() {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            rollback();
        }
        core::mem::forget(self);
    }

    /// `true` once settled out-of-band (always `false` here — settling
    /// consumes; kept for API symmetry with node-tracked guards).
    #[inline]
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.rollback.is_none()
    }
}

#[cfg(feature = "alloc")]
impl Drop for ErasedGuard {
    fn drop(&mut self) {
        if let Some(rollback) = self.rollback.take() {
            // Same `SeqCst` hygiene as the core guard (narrows, not pairs).
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            #[cfg(feature = "std")]
            {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rollback();
                }));
            }
            #[cfg(not(feature = "std"))]
            {
                rollback();
            }
        }
    }
}

#[cfg(feature = "alloc")]
impl core::fmt::Debug for ErasedGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ErasedGuard({})",
            if self.rollback.is_some() {
                "armed"
            } else {
                "disarmed"
            }
        )
    }
}
