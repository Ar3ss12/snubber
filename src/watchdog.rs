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

/// Names a rollback body that has run past `budget_ms`, from outside the
/// node lock.
///
/// A pure read: no lock, no thread, no allocation — one pass over the
/// node's phase atomics (see [`crate::RollbackPhase`]). The caller decides
/// where to poll from (a supervisor tick, a `main` loop, a watchdog task)
/// and how often; the verdict reports what was measured, not a deadline
/// the library enforces.
///
/// Returns `None` when no body is active, when `budget_ms == 0` (no
/// threshold), or while the body is still inside the budget. The elapsed
/// clock belongs to the node, so the node must arm
/// [`crate::SnubberNodeBuilder::rollback_budget_ms`] with any non-zero
/// value; otherwise `elapsed_ms` stays `0` by design and a 1 ms body is
/// indistinguishable from a wedged one. Nothing is booked into
/// [`crate::SnubberStats`] or the flight ring by this call — it is an
/// observation, and observing a stall must not change the books.
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
