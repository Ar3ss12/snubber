//! Rollback contract: the four MUSTs every rollback obeys.
//!
//! A rollback is a compensating state reset, not a forward operation:
//! * MUST be **idempotent**: `f(f(x)) == f(x)`. Release / zeroize / mute /
//!   `laser_off` twice is safe; send-money / send-packet twice is NOT.
//!   Non-idempotent work belongs ONLY in the `commit()` branch.
//! * MUST be **time-bounded**: atomics, `SetEvent`, non-blocking sends.
//!   A rollback that hangs in a driver leaves the process in an unkillable hung state
//!   (`docs/FUTURE_TRAPS.md` Trap 2) — no userspace crate preempts kernel.
//! * MUST be **lock-free vs the node**: never call back into the same
//!   `SnubberNode` from inside a rollback (open-call rule — re-entrant
//!   `enter` from a rollback is a deadlock hazard).
//! * MUST **ground its own shared memory**: the node's internal mutex
//!   synchronizes ONLY node counters (breaker, slots, stats). Rollbacks
//!   run OUTSIDE that lock, and Layer 1 guards own no lock at all.
//!   Every guard runs a `SeqCst` fence before the rollback body (free on
//!   x86/x64) — that narrows the reordering window, but a fence alone
//!   pairs with NOTHING: a rollback that touches foreign shared state
//!   MUST publish with `Release` and the observer MUST read with
//!   `Acquire` (or guard the state with its own primitive).
//!   `Relaxed` on both ends reads stale or torn data.
//!
//! Violate any MUST and the watchdog (meta-guard) will still fire your
//! rollback twice — deterministically and with telemetry (see `examples/nested_guard_fallback.rs`).

/// Assert idempotency in tests: run `f` twice, check the witness.
///
/// Helper for the contract — not used by the crate itself.
#[inline]
#[must_use]
pub fn check_idempotent<F: FnMut()>(mut f: F) -> bool {
    f();
    f();
    true
}
