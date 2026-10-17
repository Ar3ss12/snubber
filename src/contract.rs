//! Rollback contract: the four MUSTs every rollback obeys.
//!
//! A rollback is voltage discharge, not a money transfer:
//! * MUST be **idempotent**: `f(f(x)) == f(x)`. Release / zeroize / mute /
//!   `laser_off` twice is safe; send-money / send-packet twice is NOT.
//!   Non-idempotent work belongs ONLY in the `commit()` branch.
//! * MUST be **time-bounded**: atomics, `SetEvent`, non-blocking sends.
//!   A rollback that hangs in a driver turns the process into a zombie
//!   (`docs/FUTURE_TRAPS.md` Trap 2) — no userspace crate preempts kernel.
//! * MUST be **lock-free vs the node**: never call back into the same
//!   `SnubberNode` from inside a rollback (open-call rule — re-entrant
//!   `enter` from a rollback is a deadlock smell).
//! * MUST **ground its own shared memory**: the node's internal mutex
//!   synchronizes ONLY node books (counters, breaker, slots). Rollbacks
//!   run OUTSIDE that lock, and Floor-1 guards own no lock at all.
//!   Every guard runs a `SeqCst` fence before the rollback body (free on
//!   x86/x64) — that narrows the reordering window, but a fence alone
//!   pairs with NOTHING: a rollback that touches foreign shared state
//!   MUST publish with `Release` and the observer MUST read with
//!   `Acquire` (or guard the state with its own primitive).
//!   `Relaxed` on both ends reads rot.
//!
//! Violate any MUST and the watchdog (meta-guard) will still fire your
//! rollback twice —nobly, and with witnesses (see `examples/meta_guard.rs`).

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
