//! Single-entry channel gateway: multiple senders, one receiver.
//!
//! Contract: senders enqueue an intent into a bounded channel (~10 ns)
//! and return immediately. The receiver loop dequeues in order and
//! mutates state in exactly one place, inside the admission boundary.
//!
//! Rejection is synchronous and non-blocking:
//! * hard saturation: `GatewaySender::send()` returns
//!   `Err(GatewaySendError::Full)`;
//! * early pacing: once queue depth reaches the high watermark (75% of
//!   capacity), `send()` returns `Err(GatewaySendError::Throttled {
//!   retry_after_ms, depth })` with an integer-computed wait
//!   (`(depth - LWM) * service_ms`, no float, no lock, no sleep).
//!   Throttling releases only after the queue drains to the low
//!   watermark (50%) — single-threshold control would chatter at the
//!   boundary, the two-threshold latch does not.
//! * emergency path: `send_critical()` bypasses the throttle check
//!   (hard `Full` / `Disconnected` still apply).
//!
//! Inside: ONLY `std::sync::mpsc::sync_channel` + `try_send` (F14:
//! plain `Sender` has NO `try_send`, only `SyncSender` does). Never
//! blocks the sender.
use crate::{Backpressure, FaultKind};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// High watermark numerator: throttle engages at 75% of capacity.
const HWM_NUM: usize = 3;
/// Watermark denominator (shared by HWM 3/4 and LWM 1/2).
const WM_DEN: usize = 4;
/// Minimum capacity for early throttling: below 4 slots the throttle
/// engages only at hard-full (rendezvous/2-slot channels unchanged).
const THROTTLE_MIN_CAP: usize = 4;
/// Default per-item service time (ms) until the host reports a sample.
const DEFAULT_SERVICE_MS: u64 = 1;
/// Hard cap for the advised wait (60 s): saturating multiply output.
const MAX_RETRY_AFTER_MS: u64 = 60_000;

// COMPILE-TIME GATES (watermark math + threshold constants):
// * HWM (3/4) must exceed LWM (1/2) to prevent hysteresis collapse;
// * THROTTLE_MIN_CAP >= 4: below 4 slots two-threshold latch is meaningless;
// * MAX_RETRY_AFTER_MS == 60_000: hard cap documented in the public API (60 s);
// * DEFAULT_SERVICE_MS >= 1: a zero service time would produce divide-by-zero
//   in retry_after computation (`depth * service_ms`).
const _: () = {
    assert!(
        HWM_NUM * 2 > WM_DEN,
        "CRITICAL: HWM (3/4) must be strictly greater than LWM (1/2)"
    );
    assert!(
        THROTTLE_MIN_CAP >= 4,
        "CRITICAL: throttle minimum capacity must be >= 4"
    );
    assert!(
        MAX_RETRY_AFTER_MS == 60_000,
        "CRITICAL: retry cap must be exactly 60 000 ms"
    );
    assert!(
        DEFAULT_SERVICE_MS >= 1,
        "CRITICAL: default service ms must be >= 1 (zero causes divide-by-zero in retry computation)"
    );
};

/// Plain gateway counters (saturating, mutex-guarded, never panic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GatewayStats {
    /// `send` calls that landed in the channel.
    pub sent: u64,
    /// `recv` calls that delivered an intent to the receiver.
    pub admitted: u64,
    /// Intents dropped due to a full channel (counted).
    pub shed: u64,
    /// Intents paced by the HWM/LWM throttle (advised wait, not dropped by capacity).
    pub throttled: u64,
}

impl GatewayStats {
    /// All `send` outcomes observed: landed + paced + dropped.
    #[must_use]
    pub fn total_attempts(&self) -> u64 {
        self.sent
            .saturating_add(self.shed)
            .saturating_add(self.throttled)
    }

    /// Delivery ratio per-mille (0..1000), float-free telemetry:
    /// `sent * 1000 / attempts`. `1000` on an idle gateway.
    #[must_use]
    pub fn delivery_rate_per_mille(&self) -> u64 {
        let total = self.total_attempts();
        if total == 0 {
            return 1000;
        }
        self.sent.saturating_mul(1000) / total
    }

    /// Pacing ratio per-mille (0..1000): `throttled * 1000 / attempts`.
    /// Paced intents are NOT loss — they retry after the advised wait.
    #[must_use]
    pub fn paced_ratio_per_mille(&self) -> u64 {
        let total = self.total_attempts();
        if total == 0 {
            return 0;
        }
        self.throttled.saturating_mul(1000) / total
    }
}

/// Internal counters behind one short mutex (node counters untouched).
#[derive(Debug, Default)]
struct GatewayBooks {
    sent: u64,
    admitted: u64,
    shed: u64,
    throttled: u64,
}

impl GatewayBooks {
    /// Poison-tolerant counter update: a panicked peer does not corrupt
    /// `u64` counters, so recover the guard instead of dropping the tick.
    fn update(books: &Mutex<GatewayBooks>, f: impl FnOnce(&mut Self)) {
        let mut b = match books.lock() {
            Ok(b) => b,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut b);
    }
}

/// Shared throttle state: atomics only, no lock on the hot path.
#[derive(Debug)]
struct ThrottleState {
    /// Estimated queue depth (`+1` on send-ok, `-1` on recv-ok).
    ///
    /// SIGNED on purpose: a receiver may take an item before the
    /// producer's `fetch_add(1)` for it lands, so a plain unsigned
    /// counter would swallow that decrement forever and drift upward
    /// until the throttle latch wedged shut on an empty queue.
    /// `fetch_sub(1)` on an `AtomicIsize` is unconditionally honest;
    /// transient negative values mean "that many takes not yet counted"
    /// and are clamped to 0 at the `depth()` reads.
    depth: AtomicIsize,
    /// Hysteresis latch: set at HWM, cleared at/below LWM.
    throttled: AtomicBool,
    /// Consumer-side mean service time per item, ms (integer EMA).
    service_ms: AtomicU64,
    /// Precomputed high watermark (item count).
    hwm: usize,
    /// Precomputed low watermark (item count).
    lwm: usize,
}

impl ThrottleState {
    fn new(capacity: usize) -> Self {
        let (hwm, lwm) = watermarks(capacity);
        Self {
            depth: AtomicIsize::new(0),
            throttled: AtomicBool::new(false),
            service_ms: AtomicU64::new(DEFAULT_SERVICE_MS),
            hwm,
            lwm,
        }
    }

    /// Integer pacing wait: `(depth - LWM) * service_ms`, saturating, capped.
    fn retry_after_ms(&self, depth: usize) -> u64 {
        let excess = depth.saturating_sub(self.lwm) as u64;
        excess
            .saturating_mul(self.service_ms.load(Ordering::Relaxed))
            .min(MAX_RETRY_AFTER_MS)
    }
}

/// Precomputed watermarks: HWM = 75% of capacity, LWM = 50%.
/// Capacities below 4 disable early pacing entirely: HWM is set above
/// the reachable depth, so the Ok-arm latch (set at `depth >= HWM`
/// after a successful send) can never arm. The `Full` arm never
/// touches the latch — `shed` is its only counter.
#[inline]
fn watermarks(capacity: usize) -> (usize, usize) {
    if capacity < THROTTLE_MIN_CAP {
        return (capacity + 1, 0);
    }
    // 75% without overflow: `c - c/4` == `3c/4` for every `usize` and
    // saturates instead of wrapping for absurd capacities.
    (capacity - capacity / 4, capacity / 2)
}

/// Cap a raw depth estimate to the `u8` telemetry range.
#[inline]
fn depth_u8(depth: usize) -> u8 {
    depth.min(u8::MAX as usize) as u8
}

/// Why a `send` call was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GatewaySendError {
    /// Channel capacity exceeded. Non-blocking drop.
    Full,
    /// High-watermark throttle: queue at/above 75% (`depth` items),
    /// paced until it drains to 50%. Caller sleeps `retry_after_ms`
    /// instead of hammering the queue.
    Throttled {
        /// Advised wait before retry, ms: `(depth - LWM) * service_ms`.
        retry_after_ms: u64,
        /// Estimated queue depth at throttle time (capped at 255).
        depth: u8,
    },
    /// Receiver dropped; channel is broken, intent cannot be delivered.
    Disconnected,
}

impl GatewaySendError {
    /// Pacing readings for this rejection (`None` for `Disconnected`).
    #[must_use]
    pub fn backpressure(&self) -> Option<Backpressure> {
        match *self {
            Self::Full => Some(Backpressure::new(10, 255, FaultKind::BulkheadFull)),
            Self::Throttled {
                retry_after_ms,
                depth,
            } => Some(Backpressure::new(
                retry_after_ms,
                depth,
                FaultKind::BufferOverflow,
            )),
            Self::Disconnected => None,
        }
    }

    /// Advised wait before retry, ms. `0` = retry immediately.
    #[inline]
    #[must_use]
    pub fn retry_after_ms(&self) -> u64 {
        self.backpressure().map_or(0, |b| b.retry_after_ms)
    }
}

/// Why a blocking `recv` returned without a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GatewayRecvError {
    /// All senders dropped and the channel queue is empty.
    Disconnected,
}

/// Sender endpoint: cheap `Clone` (shares channel + counters). One per producer thread.
#[derive(Debug)]
pub struct GatewaySender<T> {
    tx: SyncSender<T>,
    books: Arc<Mutex<GatewayBooks>>,
    throttle: Arc<ThrottleState>,
}

/// Receiver endpoint: NOT `Clone` — one receiver owns the channel. Runs the consumer loop.
#[derive(Debug)]
pub struct GatewayHost<T> {
    rx: Receiver<T>,
    books: Arc<Mutex<GatewayBooks>>,
    throttle: Arc<ThrottleState>,
}

/// Build a channel gateway.
/// `capacity == 0` saturates to 1 (rendezvous channel, never panics).
/// Throttle watermarks: HWM = 75% of capacity, LWM = 50%;
/// capacities below 4 throttle only at hard-full.
#[must_use]
pub fn gateway<T>(capacity: usize) -> (GatewaySender<T>, GatewayHost<T>) {
    let cap = capacity.max(1);
    let (tx, rx) = sync_channel(cap);
    let books = Arc::new(Mutex::new(GatewayBooks::default()));
    let throttle = Arc::new(ThrottleState::new(cap));
    (
        GatewaySender {
            tx,
            books: Arc::clone(&books),
            throttle: Arc::clone(&throttle),
        },
        GatewayHost {
            rx,
            books,
            throttle,
        },
    )
}

impl<T> GatewaySender<T> {
    /// Enqueue one intent. Never blocks, never panics.
    ///
    /// * `Err(GatewaySendError::Full)` — channel capacity saturated.
    /// * `Err(GatewaySendError::Throttled { retry_after_ms, depth })` —
    ///   depth at/above the 75% high watermark; caller sleeps the
    ///   advised wait instead of retry-spinning. Releases only after
    ///   the queue drains to the 50% low watermark (hysteresis latch).
    pub fn send(&self, cmd: T) -> Result<(), GatewaySendError> {
        // Acquire, not Relaxed: this depth gates a refusal decision, so it
        // must not be a stale value that bypasses an armed throttle.
        let depth = self.throttle.depth.load(Ordering::Acquire).max(0) as usize;
        if self.throttle.throttled.load(Ordering::Acquire) && depth > self.throttle.lwm {
            GatewayBooks::update(&self.books, |b| {
                b.throttled = b.throttled.saturating_add(1);
            });
            return Err(GatewaySendError::Throttled {
                retry_after_ms: self.throttle.retry_after_ms(depth),
                depth: depth_u8(depth),
            });
        }
        self.send_inner(cmd)
    }

    /// Emergency path: bypasses the HWM/LWM throttle check.
    /// Hard `Full` / `Disconnected` still apply.
    pub fn send_critical(&self, cmd: T) -> Result<(), GatewaySendError> {
        self.send_inner(cmd)
    }

    fn send_inner(&self, cmd: T) -> Result<(), GatewaySendError> {
        match self.tx.try_send(cmd) {
            Ok(()) => {
                let depth = self.throttle.depth.fetch_add(1, Ordering::Relaxed) + 1;
                if depth >= self.throttle.hwm as isize {
                    self.throttle.throttled.store(true, Ordering::Release);
                }
                GatewayBooks::update(&self.books, |b| {
                    b.sent = b.sent.saturating_add(1);
                });
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                GatewayBooks::update(&self.books, |b| {
                    b.shed = b.shed.saturating_add(1);
                });
                Err(GatewaySendError::Full)
            }
            Err(TrySendError::Disconnected(_)) => Err(GatewaySendError::Disconnected),
        }
    }

    /// Throttle latch probe (set at HWM, cleared at/below LWM).
    #[must_use]
    pub fn throttled(&self) -> bool {
        self.throttle.throttled.load(Ordering::Acquire)
    }

    /// Estimated queue depth (sender-side counter, saturating at 0).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.throttle.depth.load(Ordering::Acquire).max(0) as usize
    }

    /// Snapshot the counters (`Copy` out, poison-safe default).
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        let b = match self.books.lock() {
            Ok(b) => b,
            // Poisoned books still hold valid counters — report them
            // instead of silently zeroing the whole gateway.
            Err(poisoned) => poisoned.into_inner(),
        };
        GatewayStats {
            sent: b.sent,
            admitted: b.admitted,
            shed: b.shed,
            throttled: b.throttled,
        }
    }
}

impl<T> Clone for GatewaySender<T> {
    /// Arc refcount bump only — no clone of channel data or counters.
    #[inline]
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            books: Arc::clone(&self.books),
            throttle: Arc::clone(&self.throttle),
        }
    }
}

impl<T> GatewayHost<T> {
    /// Blocking dequeue: receiver parks here until an intent arrives or all senders drop.
    pub fn recv(&self) -> Result<T, GatewayRecvError> {
        match self.rx.recv() {
            Ok(cmd) => {
                self.on_take();
                GatewayBooks::update(&self.books, |b| {
                    b.admitted = b.admitted.saturating_add(1);
                });
                Ok(cmd)
            }
            Err(_) => Err(GatewayRecvError::Disconnected),
        }
    }

    /// Non-blocking dequeue. Returns `None` if the channel is empty right now.
    #[must_use]
    pub fn try_recv(&self) -> Option<T> {
        match self.rx.try_recv() {
            Ok(cmd) => {
                self.on_take();
                GatewayBooks::update(&self.books, |b| {
                    b.admitted = b.admitted.saturating_add(1);
                });
                Some(cmd)
            }
            Err(_) => None,
        }
    }

    /// Report one processed item's service time (ms): integer EMA
    /// `new = (3*old + sample + 3)/4` driving `retry_after_ms`.
    pub fn note_service_ms(&self, sample_ms: u64) {
        let old = self.throttle.service_ms.load(Ordering::Relaxed);
        let next = old
            .saturating_mul(3)
            .saturating_add(sample_ms)
            .saturating_add(3)
            / 4;
        self.throttle
            .service_ms
            .store(next.max(1), Ordering::Relaxed);
    }

    /// Estimated queue depth (receiver-side counter, saturating at 0).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.throttle.depth.load(Ordering::Acquire).max(0) as usize
    }

    fn on_take(&self) {
        // Unconditional fetch_sub: the taken item's `fetch_add(1)` may not
        // have landed yet (receiver won the race), so a load-then-store
        // here would drop this decrement — and two concurrent takes race
        // the same way. Signed arithmetic absorbs the transient negative;
        // the latch clears whenever the post-decrement depth is at/below
        // the low watermark, which a negative prev trivially satisfies.
        let prev = self.throttle.depth.fetch_sub(1, Ordering::Relaxed);
        if prev.saturating_sub(1) <= self.throttle.lwm as isize {
            self.throttle.throttled.store(false, Ordering::Release);
        }
    }

    /// Snapshot the counters.
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        let b = match self.books.lock() {
            Ok(b) => b,
            // Poisoned books still hold valid counters — report them
            // instead of silently zeroing the whole gateway.
            Err(poisoned) => poisoned.into_inner(),
        };
        GatewayStats {
            sent: b.sent,
            admitted: b.admitted,
            shed: b.shed,
            throttled: b.throttled,
        }
    }
}
