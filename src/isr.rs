//! ISR bridge: lock-free SPSC signal ring from a hardware interrupt
//! to the main loop (`core`, no `std`, no `alloc`, no `unsafe`).
//!
//! Why atomics-only: a generic `T` ring needs `UnsafeCell<T>`, which
//! needs `unsafe` — forbidden in this crate. The cell here is a flat
//! [`AtomicU32`] (`opcode | reason | value`), so every op is safe Rust.
//! `head`/`tail` are [`AtomicU8`]: on Cortex-M0 (no `LDREX`/`STREX`)
//! byte `LDRB`/`STRB` is still single-copy atomic on the bus, and the
//! SPSC contract (ISR writes `head` only, main writes `tail` only)
//! needs no CAS at all — no `cpsid i`, no missed alarms.
//!
//! Layout: head (1 B) + 16 cells x 4 B + tail (1 B) + trip latch
//! (4 B) + shed counter (2 B) = 80 B static RAM (with align pads).
//! Push from ISR: ~2 bus cycles. Pop in the main loop, then
//! `guard.abort("hardware_isr_trip")` / `pulse.kick()` per opcode.
//!
//! Emergency trips bypass the ring through `trip_word` (bit N =
//! sensor N): `trip()` from the ISR, `take_trips()` (swap, never
//! load+clear) in the main loop BEFORE draining the ring.
//! A `Full` ring increments `shed` (telemetry dropped, trips safe).

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering};

/// Ring capacity (power of two; index mask = `CAP - 1`).
///
/// FORMAT CONTRACT: 16 slots is the published ceiling — the mask, the
/// `u8` head/tail counters and the firmware-side producer all assume it.
/// Raising it needs a lock-free-safe mask change and a version bump, not
/// just a bigger number.
pub const ISR_RING_CAP: usize = 16;
/// Index mask for the wrapping ring position.
pub const ISR_RING_MASK: u8 = (ISR_RING_CAP as u8) - 1;

const _: () = assert!(ISR_RING_CAP == 16, "ISR ring is a 16-slot contract");
const _: () = assert!(
    ISR_RING_CAP.count_ones() == 1,
    "ISR ring capacity must stay a power of two"
);

/// Opcodes packed in the signal cell (`bits 24..32`).
pub const OP_EMERGENCY_TRIP: u8 = 0x01;
/// Heartbeat tick from a hardware timer ISR.
pub const OP_PULSE: u8 = 0x02;
/// Cooperative pause request from hardware.
pub const OP_PAUSE: u8 = 0x03;

/// One packed hardware signal: `opcode:8 | reason:8 | value:16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IsrSignal {
    cell: u32,
}

impl IsrSignal {
    /// Pack a signal (`reason` = sensor id, `value` = reading/counter).
    #[inline]
    #[must_use]
    pub const fn new(opcode: u8, reason: u8, value: u16) -> Self {
        Self {
            cell: ((opcode as u32) << 24) | ((reason as u32) << 16) | (value as u32),
        }
    }

    /// Signal opcode (`OP_EMERGENCY_TRIP` / `OP_PULSE` / `OP_PAUSE`).
    #[inline]
    #[must_use]
    pub const fn opcode(self) -> u8 {
        (self.cell >> 24) as u8
    }

    /// Sensor / source id.
    #[inline]
    #[must_use]
    pub const fn reason(self) -> u8 {
        ((self.cell >> 16) & 0xFF) as u8
    }

    /// Reading or counter payload.
    #[inline]
    #[must_use]
    pub const fn value(self) -> u16 {
        (self.cell & 0xFFFF) as u16
    }

    /// Raw packed cell (for the ring store).
    #[inline]
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.cell
    }

    /// Unpack a raw cell.
    #[inline]
    #[must_use]
    pub const fn from_raw(cell: u32) -> Self {
        Self { cell }
    }
}

/// Push refusal (ring full — counted by the caller, never blocking).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IsrPushError {
    /// Ring holds `ISR_RING_CAP` unread signals; oldest is kept
    /// (fresh ISRs must not evict an unread emergency trip).
    Full,
}
/// SPSC bridge: ISR pushes, main loop pops.
///
/// Layout: `head` (ISR-owned) before the cells, `tail` (main-owned)
/// after them — 64 B of cells between the two indices, so producer
/// and consumer never share a 32 B (M7) / 64 B (x86) cache line.
/// No `repr(align(64))` padding: Cortex-M0/M3/M4 have no D-cache,
/// so 126 B of pads would buy nothing.
///
/// `trip_word` is the out-of-band emergency latch: bit N = sensor N
/// tripped. It bypasses the ring, so a trip survives a `Full` ring.
/// Drain order in the main loop is latch first, ring second.
#[derive(Debug)]
#[repr(C)]
pub struct IsrBridge {
    head: AtomicU8,
    cells: [AtomicU32; ISR_RING_CAP],
    tail: AtomicU8,
    trip_word: AtomicU32,
    shed: AtomicU16,
}

impl IsrBridge {
    /// Empty bridge (call once at boot; for a `static` bridge use
    /// `IsrBridge::new()` in a `static` initializer — the array is
    /// built cell-by-cell with no shared interior-mutable const).
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: AtomicU8::new(0),
            cells: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
            tail: AtomicU8::new(0),
            trip_word: AtomicU32::new(0),
            shed: AtomicU16::new(0),
        }
    }

    /// Latch one emergency trip bit from the ISR (bit `sensor & 31`).
    /// Single `fetch_or`: bypasses the ring, survives `Full`.
    #[inline]
    pub fn trip(&self, sensor: u8) {
        let bit = 1u32 << (sensor & 31);
        self.trip_word.fetch_or(bit, Ordering::Release);
    }

    /// Take all latched trips (main loop, call BEFORE draining the ring).
    /// `swap(0)` — never `load`+`clear`: a trip arriving before the
    /// swap is returned, one arriving after stays for the next call.
    #[inline]
    pub fn take_trips(&self) -> u32 {
        self.trip_word.swap(0, Ordering::AcqRel)
    }

    /// Non-destructive trip probe (one `Acquire` load).
    #[inline]
    pub fn has_trips(&self) -> bool {
        self.trip_word.load(Ordering::Acquire) != 0
    }

    /// Telemetry signals dropped on `Full` (saturating at `u16::MAX`).
    /// Trips are never counted here — they bypass via `trip_word`.
    #[inline]
    pub fn shed_count(&self) -> u16 {
        self.shed.load(Ordering::Relaxed)
    }

    /// Push one signal from the ISR. Fails `Full` instead of blocking.
    /// On `Full` the telemetry cell is dropped and `shed` increments
    /// (saturating); emergency trips never go through here — they use
    /// `trip()` and bypass the ring.
    ///
    /// Single producer per bridge: the ISR owns `head`, the main loop owns
    /// `tail` (SPSC). Two producers sharing one bridge would corrupt the
    /// ring regardless of the atomics.
    #[inline]
    pub fn try_push(&self, signal: IsrSignal) -> Result<(), IsrPushError> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= ISR_RING_CAP as u8 {
            // Saturating add as one CAS loop: a load-then-add let two
            // producers on the u16::MAX edge wrap the counter to zero
            // exactly when shed volume matters most.
            let mut cur = self.shed.load(Ordering::Relaxed);
            while cur < u16::MAX {
                match self.shed.compare_exchange_weak(
                    cur,
                    cur + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => cur = actual,
                }
            }
            return Err(IsrPushError::Full);
        }
        let idx = (head & ISR_RING_MASK) as usize;
        self.cells[idx].store(signal.raw(), Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Pop one signal in the main loop. `None` when empty.
    ///
    /// A malformed cell (opcode 0 is not a valid opcode) is skipped:
    /// returning `None` on it would end the caller's `while let` drain
    /// with signals still pending behind it.
    #[inline]
    pub fn try_pop(&self) -> Option<IsrSignal> {
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);
            if head == tail {
                return None;
            }
            let idx = (tail & ISR_RING_MASK) as usize;
            let cell = self.cells[idx].load(Ordering::Relaxed);
            self.tail.store(tail.wrapping_add(1), Ordering::Release);
            let signal = IsrSignal::from_raw(cell);
            if signal.opcode() == 0 {
                continue;
            }
            return Some(signal);
        }
    }

    /// Signals waiting (`head - tail`, wrapping-safe).
    #[inline]
    pub fn pending(&self) -> u8 {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail).min(ISR_RING_CAP as u8)
    }

    /// Empty probe for the hot-loop poll (`conduit.check_directive()`
    /// + `bridge.is_empty()` = two `Acquire` loads, no locks, no sleep).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tail.load(Ordering::Relaxed) == self.head.load(Ordering::Acquire)
    }
}

impl Default for IsrBridge {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

const _: () = {
    assert!(ISR_RING_CAP == 16, "IsrBridge capacity must stay 16 cells");
    assert!(
        core::mem::size_of::<IsrSignal>() == 4,
        "IsrSignal must stay one u32 cell"
    );
    assert!(
        core::mem::size_of::<IsrBridge>() == 80,
        "IsrBridge layout: 1 head + 3 pad + 64 cells + 1 tail + 3 pad + 4 trip + 2 shed + 2 pad"
    );
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// tests exercise the panic paths on purpose; the unwinds ARE the contract
mod unit_tests {
    use super::*;

    #[test]
    fn signal_packing_round_trips() {
        let s = IsrSignal::new(OP_EMERGENCY_TRIP, 0x0A, 0x1234);
        assert_eq!(s.opcode(), OP_EMERGENCY_TRIP);
        assert_eq!(s.reason(), 0x0A);
        assert_eq!(s.value(), 0x1234);
        assert_eq!(IsrSignal::from_raw(s.raw()), s);
    }

    #[test]
    fn push_pop_fifo_and_full_keeps_oldest() {
        let bridge = IsrBridge::new();
        for i in 0..ISR_RING_CAP as u16 {
            bridge
                .try_push(IsrSignal::new(OP_PULSE, 0, i))
                .expect("room");
        }
        assert_eq!(bridge.pending(), ISR_RING_CAP as u8);
        assert_eq!(
            bridge.try_push(IsrSignal::new(OP_PULSE, 0, 99)),
            Err(IsrPushError::Full)
        );
        for i in 0..ISR_RING_CAP as u16 {
            let s = bridge.try_pop().expect("signal");
            assert_eq!(s.opcode(), OP_PULSE);
            assert_eq!(s.value(), i);
        }
        assert_eq!(bridge.try_pop(), None);
        assert_eq!(bridge.pending(), 0);
    }

    #[test]
    fn head_wraps_without_branch() {
        let bridge = IsrBridge::new();
        for _ in 0..32 {
            for i in 0..ISR_RING_CAP as u16 {
                bridge
                    .try_push(IsrSignal::new(OP_PAUSE, 1, i))
                    .expect("room");
            }
            for _ in 0..ISR_RING_CAP {
                bridge.try_pop().expect("signal");
            }
        }
        assert_eq!(bridge.pending(), 0);
        bridge
            .try_push(IsrSignal::new(OP_EMERGENCY_TRIP, 7, 1))
            .expect("room");
        let s = bridge.try_pop().expect("trip survives wrap traffic");
        assert_eq!(s.opcode(), OP_EMERGENCY_TRIP);
        assert_eq!(s.reason(), 7);
    }

    #[test]
    fn trip_bypasses_full_ring_and_swap_drains_without_loss() {
        let bridge = IsrBridge::new();
        for i in 0..ISR_RING_CAP as u16 {
            bridge
                .try_push(IsrSignal::new(OP_PULSE, 0, i))
                .expect("room");
        }
        // 17th interrupt: ring full, trip bypasses through the latch.
        assert_eq!(
            bridge.try_push(IsrSignal::new(OP_EMERGENCY_TRIP, 7, 1)),
            Err(IsrPushError::Full)
        );
        assert_eq!(bridge.shed_count(), 1);
        bridge.trip(7);
        assert!(bridge.has_trips());
        // Latch first, ring second: trip word drains via swap.
        let trips = bridge.take_trips();
        assert_eq!(trips, 1u32 << 7);
        assert!(!bridge.has_trips());
        assert_eq!(bridge.take_trips(), 0);
        // Ring intact: 16 telemetry cells still queued.
        assert_eq!(bridge.pending(), ISR_RING_CAP as u8);
    }

    #[test]
    fn trip_bits_accumulate_per_sensor() {
        let bridge = IsrBridge::new();
        bridge.trip(0);
        bridge.trip(3);
        bridge.trip(31);
        assert_eq!(
            bridge.take_trips(),
            (1u32 << 0) | (1u32 << 3) | (1u32 << 31)
        );
    }

    #[test]
    fn shed_saturates_and_is_empty_tracks_ring() {
        let bridge = IsrBridge::new();
        assert!(bridge.is_empty());
        bridge
            .try_push(IsrSignal::new(OP_PULSE, 0, 1))
            .expect("room");
        assert!(!bridge.is_empty());
        bridge.try_pop().expect("signal");
        assert!(bridge.is_empty());
        for i in 0..ISR_RING_CAP as u16 {
            bridge
                .try_push(IsrSignal::new(OP_PULSE, 0, i))
                .expect("room");
        }
        for _ in 0..5 {
            let _ = bridge.try_push(IsrSignal::new(OP_PULSE, 0, 0));
        }
        assert_eq!(bridge.shed_count(), 5);
    }

    /// The saturating add is one atomic op: after the ceiling every extra
    /// shed must leave the counter at `u16::MAX`, never wrap to 0.
    #[test]
    #[cfg_attr(miri, ignore)] // 65,535 pushes: plain counter math, no model value
    fn shed_saturates_at_the_ceiling_without_wrap() {
        let bridge = IsrBridge::new();
        while bridge.try_push(IsrSignal::new(OP_PULSE, 0, 1)).is_ok() {}
        for _ in 0..=usize::from(u16::MAX) {
            let _ = bridge.try_push(IsrSignal::new(OP_PULSE, 0, 0));
        }
        assert_eq!(bridge.shed_count(), u16::MAX);
    }
}
