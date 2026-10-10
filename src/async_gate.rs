//! Async gateway: entry checkpoint for Tokio/Axum/gRPC, no runtime inside.
//!
//! The node is synchronous by design (phases A/C hold the lock for
//! nanoseconds — the Tokio scheduler never notices). This module is the
//! documented bridge: refuse-fast here, sleep in YOUR runtime there.
//!
//! * [`AsyncGate`] — thin wrapper over [`SnubberNode`](crate::SnubberNode):
//!   admission only, always [`BackpressurePolicy::AdviseOnly`], never
//!   parks a thread. Every refusal carries `retry_after_ms` + `depth`.
//! * [`AsyncSleeper`] — YOUR sleep: `tokio::time::sleep`, `async-std`,
//!   test fake. The crate never names a runtime (zero-dep stays).
//! * Rule: `BlockCaller` / `AdaptiveBlock` are FORBIDDEN behind this gate
//!   (they `std::thread::sleep` the worker — see F14). Cancellation is
//!   NOT a rollback: `select!` closes the slot honestly, half-written
//!   network state needs idempotency keys, not a destructor.
//!
//! No `tokio` in `[dependencies]`, ever. Runtimes live in YOUR binary
//! and in `examples/async_gateway.rs` (which is `no_run` without them).

use super::node::{EnterError, SnubberNode, TrackedGuard};
use super::policy::Backpressure;

/// Async entry checkpoint: [`SnubberNode`] that refuses fast, never sleeps.
///
/// Build with [`AsyncGate::new`] (asserts `AdviseOnly` at construction —
/// a node armed with `BlockCaller`/`AdaptiveBlock` is rejected with
/// `GateError::WouldBlock`, not adapted silently).
/// Then `admit()` per request: `Ok(guard)` → do async work → settle.
///
/// `Clone` is cheap (shares the node recorder); no `Debug` by design
/// (the node owns a mutex — debug-printing it would lie about state).
#[derive(Clone)]
pub struct AsyncGate {
    node: SnubberNode,
}

impl AsyncGate {
    /// Wrap a node. `Err` if the node would park a thread (async-forbidden).
    pub fn new(node: SnubberNode) -> Result<Self, GateError> {
        if node.backpressure_is_async_forbidden() {
            return Err(GateError::WouldBlock);
        }
        Ok(Self { node })
    }

    /// Wrap without checks (you audited the builder yourself).
    /// Prefer [`AsyncGate::new`]; this exists for presets you control.
    #[must_use]
    pub fn new_unchecked(node: SnubberNode) -> Self {
        Self { node }
    }

    /// Underlying node (stats, fallback, report, reset).
    #[must_use]
    pub fn node(&self) -> &SnubberNode {
        &self.node
    }

    /// Admit one request: pure synchronous checkpoint, never `.await`,
    /// never sleeps. `Ok` → guard the async work with it.
    /// `Err(e)` → map with [`classify`] / [`retry_after_ms`] below.
    pub fn admit<F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError> {
        self.node.enter(epoch, task_id, rollback)
    }

    /// Admit with membrane validation first (poison never takes a slot).
    pub fn admit_checked<P, V, F: FnOnce()>(
        &self,
        epoch: u64,
        task_id: u64,
        params: &P,
        invariant: V,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError>
    where
        V: FnOnce(&P) -> Result<(), &'static str>,
    {
        self.node
            .enter_checked(epoch, task_id, params, invariant, rollback)
    }

    /// Admit one request with automatically minted monotonic sequence coordinates.
    pub fn admit_auto<F: FnOnce()>(&self, rollback: F) -> Result<TrackedGuard<F>, EnterError> {
        self.node.enter_auto(rollback)
    }

    /// Admit with membrane validation and automatically minted sequence coordinates.
    pub fn admit_checked_auto<P, V, F: FnOnce()>(
        &self,
        params: &P,
        invariant: V,
        rollback: F,
    ) -> Result<TrackedGuard<F>, EnterError>
    where
        V: FnOnce(&P) -> Result<(), &'static str>,
    {
        self.node.enter_checked_auto(params, invariant, rollback)
    }
}

/// Why the gate refused to wrap / admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GateError {
    /// Node would park a thread (`BlockCaller`/`AdaptiveBlock` armed).
    /// Rebuild with `BackpressurePolicy::AdviseOnly`.
    WouldBlock,
}

/// YOUR sleep: implemented by the runtime you run.
///
/// In Tokio:
/// ```text
/// async fn sleep_ms(ms: u64) {
///     tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
/// }
/// ```
/// The trait below only names the shape so all runtimes share one gate.
pub trait AsyncSleeper {
    /// Sleep `ms` without blocking an OS thread.
    fn sleep_ms(ms: u64) -> impl std::future::Future<Output = ()>;
}

/// Classify a refusal for an HTTP/gRPC mapper (exhaustive, no `_`).
#[inline]
#[must_use]
pub fn classify(err: &EnterError) -> Refusal {
    match *err {
        EnterError::BreakerOpen => Refusal::FailFast,
        EnterError::BulkheadFull | EnterError::RateLimited | EnterError::QueueFull => {
            Refusal::Backoff
        }
        EnterError::StaleEpoch | EnterError::QueueTtlExpired => Refusal::Ignore,
        EnterError::InvariantBreach(_) | EnterError::PayloadBudgetExceeded(_) => {
            Refusal::BadRequest
        }
    }
}

/// Async outcome of one admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// Sick downstream — serve fallback, `200` with degraded body.
    FailFast,
    /// Wall pressure — sleep `retry_after_ms` in YOUR runtime, then `429`/`503`.
    Backoff,
    /// Late packet — `200` ignored, nothing to pace.
    Ignore,
    /// Bad input — `400` with the invariant reason.
    BadRequest,
}

/// Advised wait for a refusal, ms (`0` = nothing to pace — fix the input).
#[inline]
#[must_use]
pub fn retry_after_ms(err: &EnterError) -> u64 {
    err.retry_after_ms()
}

/// Wall readings if this refusal IS pressure (`None` = wrong door).
#[inline]
#[must_use]
pub fn backpressure_of(err: &EnterError) -> Option<Backpressure> {
    err.backpressure()
}

/// Bulkhead wait ownership this node was built with (for the assert).
#[inline]
#[must_use]
pub fn backpressure_kind(node: &SnubberNode) -> &'static str {
    node.backpressure_policy_kind()
}
