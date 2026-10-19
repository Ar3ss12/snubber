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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
        self.sent.saturating_add(self.shed).saturating_add(self.throttled)
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

/// Shared throttle state: atomics only, no lock on the hot path.
#[derive(Debug)]
struct ThrottleState {
    /// Estimated queue depth (`+1` on send-ok, `-1` on recv-ok).
    depth: AtomicUsize,
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
            depth: AtomicUsize::new(0),
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
/// Capacities below 4 engage throttling only at hard-full: HWM is set
/// above capacity so the latch can only arm on a saturated `try_send`
/// path that reports `Full` (early pacing stays disabled).
#[inline]
fn watermarks(capacity: usize) -> (usize, usize) {
    if capacity < THROTTLE_MIN_CAP {
        return (capacity + 1, 0);
    }
    (capacity * HWM_NUM / WM_DEN, capacity / 2)
}

/// Cap a raw depth estimate to the `u8` telemetry range.
#[inline]
fn depth_u8(depth: usize) -> u8 {
    depth.min(u8::MAX as usize) as u8
}

/// Why a `send` call was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
        let depth = self.throttle.depth.load(Ordering::Relaxed);
        if self.throttle.throttled.load(Ordering::Acquire) && depth > self.throttle.lwm {
            if let Ok(mut b) = self.books.lock() {
                b.throttled = b.throttled.saturating_add(1);
            }
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
                if depth >= self.throttle.hwm {
                    self.throttle.throttled.store(true, Ordering::Release);
                }
                if let Ok(mut b) = self.books.lock() {
                    b.sent = b.sent.saturating_add(1);
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                if let Ok(mut b) = self.books.lock() {
                    b.shed = b.shed.saturating_add(1);
                }
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

    /// Estimated queue depth (sender-side counter, saturating).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.throttle.depth.load(Ordering::Relaxed)
    }

    /// Snapshot the counters (`Copy` out, poison-safe default).
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        self.books
            .lock()
            .map_or(GatewayStats::default(), |b| GatewayStats {
                sent: b.sent,
                admitted: b.admitted,
                shed: b.shed,
                throttled: b.throttled,
            })
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
                if let Ok(mut b) = self.books.lock() {
                    b.admitted = b.admitted.saturating_add(1);
                }
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
                if let Ok(mut b) = self.books.lock() {
                    b.admitted = b.admitted.saturating_add(1);
                }
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

    /// Estimated queue depth (receiver-side counter, saturating).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.throttle.depth.load(Ordering::Relaxed)
    }

    fn on_take(&self) {
        let prev = self.throttle.depth.load(Ordering::Relaxed);
        if prev > 0 {
            self.throttle.depth.store(prev - 1, Ordering::Relaxed);
        }
        if prev.saturating_sub(1) <= self.throttle.lwm {
            self.throttle.throttled.store(false, Ordering::Release);
        }
    }

    /// Snapshot the counters.
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        self.books
            .lock()
            .map_or(GatewayStats::default(), |b| GatewayStats {
                sent: b.sent,
                admitted: b.admitted,
                shed: b.shed,
                throttled: b.throttled,
            })
    }
}
