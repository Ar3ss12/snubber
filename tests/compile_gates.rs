//! Compile-time gates: thread-safety the compiler enforces, not tests.
//!
//! Zero `#[test]` functions here on purpose — this file contributes ZERO
//! to the test count. Every block below is a `const` guillotine: break the
//! invariant and the BUILD dies with the CRITICAL line, long before any
//! runtime could observe the rot. (Pattern: Alex Crichton's
//! `futures-cpupool` send/sync probes, dependency-free.)
//!
//! Gated coverage:
//! * always: `SnapshotV1` (lives in `diagnostics`, compiled on every
//!   feature set — the cold frame must cross threads on any target);
//! * `std` only: `SnubberNode`, `StateDigest`, `ProcessToken`,
//!   `StaticNode<4, 16>` (their modules are `std`-gated; `no_std` builds
//!   never see these probes).
//!
//! Deliberately NOT pinned: `SnubberGuard<F>` / `ErasedGuard` — generic
//! single-owner guards whose thread semantics follow the CLOSURE, not the
//! struct. Pinning `SnubberGuard<fn()>` would prove nothing about
//! `SnubberGuard<impl FnOnce() + Send>` and would only add noise.
//! Deliberately NOT pinned: `ScopeToken` — it is `!Send + !Sync` BY DESIGN
//! (the seal). Its proof is the `compile_fail` doctest on
//! `scoped_sealed`, not a positive bound.

const fn assert_send_sync<T: Send + Sync>() {}

const _: () = {
    assert_send_sync::<snubber::SnapshotV1>();
};

#[cfg(feature = "std")]
const _: () = {
    assert_send_sync::<snubber::SnubberNode>();
    assert_send_sync::<snubber::StateDigest>();
    assert_send_sync::<snubber::ProcessToken>();
    assert_send_sync::<snubber::StaticNode<4, 16>>();
    assert_send_sync::<snubber::Protected<snubber::TransactionHeader, fn()>>();
};
