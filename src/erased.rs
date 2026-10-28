//! Closure-capturing dynamic commands and type-erased rollback slots.
//!
//! While core `FaultAction::Custom(fn())` remains `Copy` and zero-allocation
//! for bare-metal firmware, this module provides heap-backed capturing commands
//! when the `alloc` feature is available.
//!
//! # Components
//! - [`ClosurePolicy`]: Capturing `Box<dyn Fn() + Send + Sync>` executed outside
//!   the node lock with non-panicking, real-time safety guarantees.
//! - [`ErasedGuard`]: Type-erased rollback guard. Generic `SnubberGuard<F>`
//!   cannot be held directly in struct fields due to unique anonymous closure types.
//!   `ErasedGuard` boxes the rollback closure once at initialization, exposing a
//!   concrete, nameable type suitable for long-lived workers and structs.
//!
//! Enabled by default via the `std` feature (`std = ["alloc"]`).
//! Bare-metal firmware targets opt out via `default-features = false`.

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
use alloc::boxed::Box;

/// Boxed command with captured context.
///
/// ```rust
/// use snubber::ClosurePolicy;
/// use std::sync::atomic::{AtomicU32, Ordering};
/// use std::sync::Arc;
///
/// let counter = Arc::new(AtomicU32::new(0));
/// let c = counter.clone();
/// let policy = ClosurePolicy::new(move || {
///     c.fetch_add(1, Ordering::SeqCst);
/// });
/// policy.run();
/// assert_eq!(counter.load(Ordering::SeqCst), 1);
/// ```
#[cfg(feature = "alloc")]
pub struct ClosurePolicy {
    cmd: Box<dyn Fn() + Send + Sync>,
}

#[cfg(feature = "alloc")]
impl ClosurePolicy {
    /// Boxes a capturing closure as an executable node command.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self { cmd: Box::new(f) }
    }

    /// Executes the captured command outside the node lock.
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

/// Type-erased rollback guard for struct storage and long-lived tasks.
///
/// Erases the closure type once via heap allocation, providing a concrete,
/// nameable type that fits into struct fields, collections, and state machines:
///
/// ```rust,no_run
/// use snubber::{ErasedGuard, SnubberNode};
///
/// struct DaemonState {
///     hold: Option<ErasedGuard>, // Feasible without generic type parameters
/// }
///
/// let node = SnubberNode::builder("worker").build();
/// let mut state = DaemonState { hold: None };
///
/// state.hold = Some(node.probe_erased(1, 1, || release_hardware()).unwrap());
/// // If the task fails or drops here, hardware is released via Drop.
/// state.hold.take().map(|g| g.commit()); // Clean completion
///
/// # fn release_hardware() {}
/// ```
#[cfg(feature = "alloc")]
#[must_use = "Dropping an ErasedGuard immediately executes rollback! Bind to a named variable: `let _guard = ...`, NEVER use `let _ = ...`"]
pub struct ErasedGuard {
    /// The whole settle path in one allocation: `new` / `with_commit_hook`
    /// box a `TwoSlots`, the node boxes one `ErasedTransaction`. Two
    /// separate boxed closures cost two allocations per erased admission.
    slots: Option<Box<dyn ErasedSettle>>,
}

#[cfg(feature = "alloc")]
impl ErasedGuard {
    /// Creates a type-erased guard owning a single rollback closure.
    pub fn new<F>(rollback: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            slots: Some(Box::new(TwoSlots::<F, fn()> {
                rollback: Some(rollback),
                on_commit: None,
            })),
        }
    }

    /// Creates a type-erased guard with both a rollback closure and a success-path commit hook.
    pub fn with_commit_hook<R, C>(rollback: R, on_commit: C) -> Self
    where
        R: FnOnce() + Send + 'static,
        C: FnOnce() + Send + 'static,
    {
        Self {
            slots: Some(Box::new(TwoSlots {
                rollback: Some(rollback),
                on_commit: Some(on_commit),
            })),
        }
    }

    /// Wraps a pre-boxed settle path. Crate-internal: the node's erased
    /// admissions keep the node handle and the rollback in the same
    /// allocation instead of boxing two closures.
    #[cfg(feature = "std")]
    #[inline]
    pub(crate) fn from_settle(slots: Box<dyn ErasedSettle>) -> Self {
        Self { slots: Some(slots) }
    }

    /// Disarms the rollback, executes the optional commit hook, and consumes the guard.
    ///
    /// The box is taken out and settled, so the normal `Drop` below sees
    /// `None` and does nothing — no `mem::forget` is needed, and forgetting
    /// would skip drop glue for every field (the `TrackedGuard` leak of
    /// 0.5.2).
    ///
    /// Memory ordering: emits a `SeqCst` fence prior to disarming, matching [`crate::SnubberGuard`].
    #[inline]
    pub fn commit(mut self) {
        if let Some(slots) = self.slots.take() {
            slots.commit();
        }
    }

    /// Immediately triggers the rollback closure and consumes the guard.
    ///
    /// Under `std` a panicking rollback is swallowed exactly as in `Drop`
    /// — a planned abort never escalates into a double-panic process
    /// abort. Without `std` there is no catcher (the rollback panic during
    /// unwind aborts the process — a Rust rule, documented in
    /// [`crate::SnubberGuard`]).
    #[inline]
    pub fn abort(mut self, _reason: &'static str) {
        if let Some(slots) = self.slots.take() {
            #[cfg(feature = "std")]
            {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    slots.rollback();
                }));
            }
            #[cfg(not(feature = "std"))]
            {
                slots.rollback();
            }
        }
    }

    /// Returns `true` if the guard has been settled.
    ///
    /// Always returns `false` on a live instance since settling consumes `self`.
    /// Maintained for interface symmetry across guard implementations.
    #[inline]
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.slots.is_none()
    }
}

#[cfg(feature = "alloc")]
impl Drop for ErasedGuard {
    fn drop(&mut self) {
        if let Some(slots) = self.slots.take() {
            #[cfg(feature = "std")]
            {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    slots.rollback();
                }));
            }
            #[cfg(not(feature = "std"))]
            {
                slots.rollback();
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
            if self.slots.is_some() {
                "armed"
            } else {
                "disarmed"
            }
        )
    }
}

/// The whole settle path behind an [`ErasedGuard`]: one allocation instead
/// of one box per closure.
#[cfg(feature = "alloc")]
pub(crate) trait ErasedSettle: Send {
    /// Failure path: drop the commit hook, run the rollback.
    fn rollback(self: Box<Self>);

    /// Success path: drop the rollback, run the commit hook / node books.
    fn commit(self: Box<Self>);
}

/// Guard whose rollback and commit hook are separate closures
/// ([`ErasedGuard::new`] / [`ErasedGuard::with_commit_hook`]).
#[cfg(feature = "alloc")]
struct TwoSlots<R, C> {
    rollback: Option<R>,
    on_commit: Option<C>,
}

#[cfg(feature = "alloc")]
impl<R, C> ErasedSettle for TwoSlots<R, C>
where
    R: FnOnce() + Send + 'static,
    C: FnOnce() + Send + 'static,
{
    fn rollback(mut self: Box<Self>) {
        let _ = self.on_commit.take();
        if let Some(rollback) = self.rollback.take() {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            rollback();
        }
    }

    fn commit(mut self: Box<Self>) {
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
    }
}
