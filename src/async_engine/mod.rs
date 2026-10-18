//! Asynchronous primitives and adapters for `snubber`.
//!
//! Provides zero-runtime futures, task waker queues, pacing with custom sleep,
//! async settlement, and gateway policy evaluation for asynchronous runtimes (e.g. Tokio).

pub mod entry;
pub mod gate;
pub mod lease;
pub mod pace;
pub mod settle;

pub use entry::EnterFuture;
pub use gate::{backpressure_of, classify, retry_after_ms, AsyncGate, GateError, Refusal};
pub use lease::{AsyncLease, LeaseChannel, LeaseEvent};
pub use pace::enter_paced_async;
pub use settle::settle_async;
