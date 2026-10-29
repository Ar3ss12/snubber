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
//! - [`SlotKind`] (`ProbeSlot` / `HoldingSlot`): what the admission did with
//!   the bulkhead slot, carried as a type parameter so a signature that needs
//!   the slot cannot be satisfied by a guard whose slot was already released.
//!
//! Enabled by default via the `std` feature (`std = ["alloc"]`).
//! Bare-metal firmware targets opt out via `default-features = false`.

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
use alloc::boxed::Box;

#[cfg(feature = "alloc")]
use core::marker::PhantomData;

/// Closed set of admission lifetimes: a caller can name one of the two kinds,
/// never mint a third that lies about its slot.
#[cfg(feature = "alloc")]
mod sealed {
    /// Implemented only by [`ProbeSlot`](super::ProbeSlot) and
    /// [`HoldingSlot`](super::HoldingSlot).
    pub trait Sealed {}
}

/// What an erased admission did with the bulkhead slot — the type parameter
/// of [`ErasedGuard`].
///
/// The admission methods return the kind they actually take, so a signature
/// that needs one lifetime cannot be satisfied by the other.
#[cfg(feature = "alloc")]
pub trait SlotKind: sealed::Sealed + 'static {
    /// `true` when the slot stays booked until the guard settles.
    const HOLDS_SLOT: bool;
    /// Static text for diagnostics (`"probe"` / `"holding"`).
    const CODE: &'static str;
}

/// The bulkhead slot was released at ADMISSION: `max_in_flight` bounds
/// admissions, not live probe guards.
///
/// Returned by [`SnubberNode::probe_erased`](crate::SnubberNode::probe_erased),
/// [`SnubberNode::probe_erased_auto`](crate::SnubberNode::probe_erased_auto)
/// and [`SnubberNode::enter_supervised_erased`](crate::SnubberNode::enter_supervised_erased).
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeSlot {}

#[cfg(feature = "alloc")]
impl sealed::Sealed for ProbeSlot {}

#[cfg(feature = "alloc")]
impl SlotKind for ProbeSlot {
    const HOLDS_SLOT: bool = false;
    const CODE: &'static str = "probe";
}

/// The bulkhead slot stays booked until `commit()` / `abort()` / `Drop`:
/// `max_in_flight` is a real ceiling for live transactions.
///
/// Returned by
/// [`SnubberNode::enter_erased_holding`](crate::SnubberNode::enter_erased_holding)
/// and [`SnubberNode::enter_erased_holding_auto`](crate::SnubberNode::enter_erased_holding_auto).
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HoldingSlot {}

#[cfg(feature = "alloc")]
impl sealed::Sealed for HoldingSlot {}

#[cfg(feature = "alloc")]
impl SlotKind for HoldingSlot {
    const HOLDS_SLOT: bool = true;
    const CODE: &'static str = "holding";
}

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
/// use snubber::{ErasedGuard, ProbeSlot, SnubberNode};
///
/// struct DaemonState {
///     // Feasible without a closure type: one nameable type, and the
///     // parameter says what the admission did with the slot.
///     hold: Option<ErasedGuard<ProbeSlot>>,
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
///
/// The type parameter names what the admission did with the bulkhead slot;
/// see [`SlotKind`]. A guard built by [`ErasedGuard::new`] carries the
/// default, [`HoldingSlot`]: it owns its obligation until settled and books
/// no admission.
///
/// A function that needs the slot to stay booked says so in its signature,
/// and a probe guard no longer compiles there:
///
/// ```compile_fail
/// use snubber::{ErasedGuard, HoldingSlot, SnubberNode};
///
/// fn keep_until_settle(_hold: ErasedGuard<HoldingSlot>) {}
///
/// let node = SnubberNode::builder("slots").build();
/// let probe = node.probe_erased(1, 1, || {}).expect("admitted");
/// keep_until_settle(probe);
/// ```
///
/// The reverse pairing refuses too — a holding guard is not a probe:
///
/// ```compile_fail
/// use snubber::{ErasedGuard, ProbeSlot, SnubberNode};
///
/// fn count_live_admissions(_probe: ErasedGuard<ProbeSlot>) {}
///
/// let node = SnubberNode::builder("slots").build();
/// let holding = node.enter_erased_holding(1, 1, || {}).expect("admitted");
/// count_live_admissions(holding);
/// ```
#[cfg(feature = "alloc")]
#[must_use = "Dropping an ErasedGuard immediately executes rollback! Bind to a named variable: `let _guard = ...`, NEVER use `let _ = ...`"]
pub struct ErasedGuard<K: SlotKind = HoldingSlot> {
    /// The whole settle path in one allocation: `new` / `with_commit_hook`
    /// box a `TwoSlots`, the node boxes one `ErasedTransaction`. Two
    /// separate boxed closures cost two allocations per erased admission.
    slots: Option<Box<dyn ErasedSettle>>,
    /// `fn() -> K` keeps the guard `Send`/`Sync` for every kind and ties the
    /// parameter to the type without claiming to own a `K`.
    kind: PhantomData<fn() -> K>,
}

/// Guards you build yourself are [`HoldingSlot`]: they book no admission, so
/// there is no probe to name — the obligation is yours until you settle.
#[cfg(feature = "alloc")]
impl ErasedGuard<HoldingSlot> {
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
            kind: PhantomData,
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
            kind: PhantomData,
        }
    }
}

#[cfg(feature = "alloc")]
impl<K: SlotKind> ErasedGuard<K> {
    /// Wraps a pre-boxed settle path. Crate-internal: the node's erased
    /// admissions keep the node handle and the rollback in the same
    /// allocation instead of boxing two closures.
    #[cfg(feature = "std")]
    #[inline]
    pub(crate) fn from_settle(slots: Box<dyn ErasedSettle>) -> Self {
        Self {
            slots: Some(slots),
            kind: PhantomData,
        }
    }

    /// Settled guard with no rollback and no booked admission — what a
    /// pass-through node hands back. Crate-internal: it is the one
    /// constructor that owes nothing, so it must not be reachable from a
    /// caller who believes a guard protects something.
    #[cfg(feature = "std")]
    #[inline]
    pub(crate) fn settled_noop() -> Self {
        Self {
            slots: None,
            kind: PhantomData,
        }
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
impl<K: SlotKind> Drop for ErasedGuard<K> {
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
impl<K: SlotKind> core::fmt::Debug for ErasedGuard<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ErasedGuard<{}>({})",
            K::CODE,
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
