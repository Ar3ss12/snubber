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
