//! Fault-isolated callback execution (Panic Boundary).
//!
//! Guarantees that a panic inside external or user-provided code
//! (telemetry plugins, observers, diagnostic hooks) does not abort
//! subsequent cleanups or compromise caller invariants.
//!
//! # Architecture & Ordering
//! - Open-Call Pattern: The node mutex is always released *before* calling
//!   `isolate_once`, preventing lock poisoning across panic boundaries.
//! - Overhead: In `std` environments, isolation incurs exactly one `catch_unwind`
//!   per invocation.
//! - Zero-Cost in `no_std`: In bare-metal environments without an unwinder,
//!   the closure is invoked directly and the contract requires foreign code
//!   to remain non-panicking.
//!
//! The module also carries [`poll_rollback`], the outside-the-lock observer
//! that names a rollback body which never returned (a read-only companion
//! to [`isolate_once`], not another thread).
#[cfg(feature = "std")]
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Executes a foreign closure behind an isolated panic boundary.
///
/// Returns `true` if the closure completed successfully, or `false`
/// if execution panicked and was intercepted.
///
/// Guarantees that stack unwinding does not propagate past this call under `std`.
#[inline]
pub fn isolate_once<F: FnOnce()>(cmd: F) -> bool {
    #[cfg(feature = "std")]
    {
        catch_unwind(AssertUnwindSafe(cmd)).is_ok()
    }
    #[cfg(not(feature = "std"))]
    {
        cmd();
        true
    }
}

/// A rollback body that has outlived an observer's budget.
///
/// Minted by [`poll_rollback`], never booked by the node itself: the node
/// records its OWN overruns ([`crate::FaultKind::RollbackStall`]) only when
/// a body RETURNS, so a body that never returns produces no such record.
/// This verdict is what remains observable while the body is still running.
///
/// `epoch` and `task_id` identify the most recently started body, matching
/// [`crate::RollbackPhase`]; `elapsed_ms` is the age of the OLDEST active
/// body, because that is the one a watcher is measuring. With one body —
/// the wedged case — both refer to the same transaction.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RollbackStall {
    /// Epoch of the most recently started rollback body.
    pub epoch: u64,
    /// Task id of the most recently started rollback body.
    pub task_id: u64,
    /// Age of the oldest active body when this verdict was read, ms.
    pub elapsed_ms: u64,
    /// Budget the observer asked about, ms.
    pub budget_ms: u64,
}

/// Names a rollback body that has run past a threshold the CALLER sets.
///
/// The 0.6 shape, kept because a public signature is a contract: the budget
/// is a parameter, `0` means "not armed here". It cannot manufacture age —
/// the node's clock must still be armed, otherwise `elapsed_ms` stays `0`
/// and nothing is ever named. Prefer [`poll_rollback_node`] from a
/// supervisor tick: a caller-side constant is exactly the number that drifts
/// away from the builder.
#[cfg(feature = "std")]
#[inline]
pub fn poll_rollback(node: &crate::SnubberNode, budget_ms: u64) -> Option<RollbackStall> {
    if budget_ms == 0 {
        return None;
    }
    let phase = node.rollback_phase();
    if !phase.active || phase.elapsed_ms <= budget_ms {
        return None;
    }
    Some(RollbackStall {
        epoch: phase.epoch,
        task_id: phase.task_id,
        elapsed_ms: phase.elapsed_ms,
        budget_ms,
    })
}

/// Names a rollback body that has run past the budget THE NODE WAS BUILT
/// WITH, from outside the node lock.
///
/// This is the form to call from a supervisor tick: the threshold is read
/// from the node ([`crate::SnubberNode::rollback_budget_ms`]) on the same
/// pass that reads the phase, so a watcher cannot outlive a builder change
/// and time a body against a number the node never armed. A node built
/// without `rollback_budget_ms` has no threshold — `None`, never an
/// invented age.
///
/// A pure read: no lock, no thread, no allocation. Nothing is booked into
/// [`crate::SnubberStats`] or the flight ring — observing a stall must not
/// change the books. Use [`poll_rollback`] only when the caller's threshold
/// deliberately differs from the builder's.
#[cfg(feature = "std")]
#[inline]
pub fn poll_rollback_node(node: &crate::SnubberNode) -> Option<RollbackStall> {
    poll_rollback(node, node.rollback_budget_ms())
}
