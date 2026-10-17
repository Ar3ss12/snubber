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
/// state.hold = Some(node.enter_erased(1, 1, || release_hardware()).unwrap());
/// // If the task fails or drops here, hardware is released via Drop.
/// state.hold.take().map(|g| g.commit()); // Clean completion
///
/// # fn release_hardware() {}
/// ```
#[cfg(feature = "alloc")]
#[must_use = "Dropping an ErasedGuard immediately executes rollback! Bind to a named variable: `let _guard = ...`, NEVER use `let _ = ...`"]
pub struct ErasedGuard {
    rollback: Option<Box<dyn FnOnce() + Send + 'static>>,
    on_commit: Option<Box<dyn FnOnce() + Send + 'static>>,
}

#[cfg(feature = "alloc")]
impl ErasedGuard {
    /// Creates a type-erased guard owning a single rollback closure.
    pub fn new<F>(rollback: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            rollback: Some(Box::new(rollback)),
            on_commit: None,
        }
    }

    /// Creates a type-erased guard with both a rollback closure and a success-path commit hook.
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

    /// Disarms the rollback, executes the optional commit hook, and consumes the guard.
    ///
    /// Memory ordering: emits a `SeqCst` fence prior to disarming, matching [`crate::SnubberGuard`].
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
        // Bypass Drop; resources are already explicitly settled
        core::mem::forget(self);
    }

    /// Immediately triggers the rollback closure and consumes the guard.
    #[inline]
    pub fn abort(mut self, _reason: &'static str) {
        let _ = self.on_commit.take();
        if let Some(rollback) = self.rollback.take() {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            rollback();
        }
        // Bypass Drop; rollback has already executed
        core::mem::forget(self);
    }

    /// Returns `true` if the guard has been settled.
    ///
    /// Always returns `false` on a live instance since settling consumes `self`.
    /// Maintained for interface symmetry across guard implementations.
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
