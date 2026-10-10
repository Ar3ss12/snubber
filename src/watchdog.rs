//! Watchdog isolation: one rotten observer must never silence the rest.
//!
//! The rule is absolute: every FOREIGN call (custom command, hook) gets
//! its own panic boundary under `std`. The node lock is already dropped
//! at this point (open-call), so isolation here costs one `catch_unwind`
//! per call and buys a chain that cannot be beheaded by a single rotten
//! link — telemetry plugin dies, emergency reset still fires, console
//! line still prints.
//!
//! Without `std` the contract is "foreign code never panics" (documented
//! on the guard types, enforced by nothing — `core` has no unwinder).

#[cfg(feature = "std")]
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Run a foreign `FnOnce` command behind a panic boundary.
///
/// Returns `true` if the command ran clean, `false` if it was swallowed.
/// Never unwinds under `std`; direct call without it.
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
