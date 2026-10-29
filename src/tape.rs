//! A fixed ring of the last events before a crash — no allocation, no clock.
//!
//! The node's flight recorder ([`SnubberNode::dump_flight`](crate::SnubberNode::dump_flight))
//! is a `VecDeque` of rich slots, and it lives behind `std` for a reason:
//! the moment it is most wanted is the moment the process is out of memory,
//! and a recorder that allocates on the way to a crash is the first thing to
//! fail. [`FlightTape<N>`] is the same idea with none of that: a fixed array
//! of `(epoch, task_id, code)` triples that overwrites its oldest entry and
//! works on firmware.
//!
//! The tape names WHAT happened in which epoch and task. It carries no file,
//! line or text: those are `&'static str` in [`FlightSlot`](crate::FlightSlot)
//! and belong to the `std` recorder. `code` is the caller's own vocabulary —
//! a `FaultKind::index()`, a transition number, an `OP_*` byte — the crate
//! never interprets it.

/// One tape cell: `(epoch, task_id, code)`, 24 B on 64-bit, `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TapeEntry {
    /// Epoch of the task this entry belongs to.
    pub epoch: u64,
    /// Task identity inside that epoch.
    pub task_id: u64,
    /// Caller-defined event code (a `FaultKind::index()`, a transition
    /// number, a wire opcode) — stored, never interpreted.
    pub code: u32,
}

impl TapeEntry {
    /// Empty cell (all zeros); every field is public, so this is just a
    /// shorthand for the array initializer.
    pub const EMPTY: Self = Self {
        epoch: 0,
        task_id: 0,
        code: 0,
    };

    /// Construct one entry.
    #[must_use]
    pub const fn new(epoch: u64, task_id: u64, code: u32) -> Self {
        Self {
            epoch,
            task_id,
            code,
        }
    }
}

/// Fixed-capacity tape of the most recent [`TapeEntry`] values.
///
/// `N` is the number of events the tape remembers; recording past it
/// overwrites the oldest cell, so the tape always holds the LAST `N` events
/// in order. `N = 0` is legal and records nothing (a tape sized from a
/// config value must not turn a zero into a panic).
///
/// ```
/// use snubber::FlightTape;
///
/// let mut tape: FlightTape<3> = FlightTape::new();
/// for code in 1u32..=4 {
///     tape.record_code(1, 7, code);
/// }
/// let codes: Vec<u32> = tape.iter().map(|e| e.code).collect();
/// assert_eq!(codes, vec![2, 3, 4]);              // event 1 was overwritten
/// assert_eq!(tape.newest().map(|e| e.code), Some(4));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlightTape<const N: usize> {
    slots: [TapeEntry; N],
    next: usize,
    len: usize,
}

impl<const N: usize> Default for FlightTape<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FlightTape<N> {
    /// Empty tape.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [TapeEntry::EMPTY; N],
            next: 0,
            len: 0,
        }
    }

    /// Record one event, overwriting the oldest cell when the tape is full.
    pub fn record(&mut self, entry: TapeEntry) {
        if N == 0 {
            return;
        }
        self.slots[self.next] = entry;
        self.next = (self.next + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Record `(epoch, task_id, code)` without building an entry at the call
    /// site (the hot path of an error handler).
    pub fn record_code(&mut self, epoch: u64, task_id: u64, code: u32) {
        self.record(TapeEntry::new(epoch, task_id, code));
    }

    /// Index of the oldest live cell in `slots`.
    #[inline]
    const fn start(&self) -> usize {
        if self.len < N {
            0
        } else {
            self.next
        }
    }

    /// Entries from oldest to newest — the order they happened.
    pub fn iter(&self) -> impl Iterator<Item = &TapeEntry> {
        let start = self.start();
        let len = self.len;
        (0..len).map(move |i| &self.slots[(start + i) % N])
    }

    /// Entry at age `index` (`0` = oldest live entry).
    #[must_use]
    pub fn at(&self, index: usize) -> Option<&TapeEntry> {
        if index < self.len {
            Some(&self.slots[(self.start() + index) % N])
        } else {
            None
        }
    }

    /// Most recent entry, if any.
    #[must_use]
    pub fn newest(&self) -> Option<&TapeEntry> {
        if self.len == 0 {
            return None;
        }
        Some(&self.slots[(self.next + N - 1) % N])
    }

    /// Oldest live entry, if any.
    #[must_use]
    pub fn oldest(&self) -> Option<&TapeEntry> {
        if self.len == 0 {
            None
        } else {
            Some(&self.slots[self.start()])
        }
    }

    /// Most recent entry carrying `code` — the "what was the last thing this
    /// task did before it died" query, walked newest first.
    #[must_use]
    pub fn last_with_code(&self, code: u32) -> Option<&TapeEntry> {
        for age in 0..self.len {
            let slot = &self.slots[(self.next + N - 1 - age) % N];
            if slot.code == code {
                return Some(slot);
            }
        }
        None
    }

    /// Live entries (`0..=N`).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` when no entry was recorded yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `true` when the tape holds `N` entries (`N = 0` is never full).
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == N && N != 0
    }

    /// Compile-time capacity (`N`).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Drop every entry and rewind the cursor.
    pub fn clear(&mut self) {
        self.slots = [TapeEntry::EMPTY; N];
        self.next = 0;
        self.len = 0;
    }

    /// Raw cells in storage order plus the write cursor and live length.
    /// The wire codec writes all three so a decode restores the ring
    /// byte-for-byte — re-deriving the ring from event order would lose
    /// the difference between two tapes that hold the same events.
    pub(crate) const fn raw(&self) -> (&[TapeEntry; N], usize, usize) {
        (&self.slots, self.next, self.len)
    }

    /// Rebuild from storage order. `next` is wrapped and `len` clamped to
    /// `N`, so a hostile or corrupted frame cannot index outside the ring.
    pub(crate) fn from_raw(slots: [TapeEntry; N], next: usize, len: usize) -> Self {
        if N == 0 {
            return Self::new();
        }
        Self {
            slots,
            next: next % N,
            len: len.min(N),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_short_tape_reads_oldest_first() {
        let mut tape: FlightTape<5> = FlightTape::new();
        for code in [1u32, 2, 3] {
            tape.record_code(7, 100 + u64::from(code), code);
        }
        assert!(
            tape.iter().map(|e| e.code).eq([1u32, 2, 3]),
            "the tape reads in event order"
        );
        assert_eq!(tape.len(), 3);
        assert!(!tape.is_full());
        assert_eq!(tape.capacity(), 5);
        assert_eq!(tape.oldest().map(|e| e.code), Some(1));
        assert_eq!(tape.newest().map(|e| e.code), Some(3));
        assert_eq!(tape.at(1).map(|e| e.task_id), Some(102));
        assert!(tape.at(3).is_none());
    }

    #[test]
    fn a_full_tape_overwrites_the_oldest() {
        let mut tape: FlightTape<3> = FlightTape::new();
        for code in 1u32..=5 {
            tape.record_code(u64::from(code), u64::from(code) * 10, code);
        }
        assert!(
            tape.iter().map(|e| e.code).eq([3u32, 4, 5]),
            "only the last N events survive"
        );
        assert_eq!(tape.len(), 3);
        assert!(tape.is_full());
        assert_eq!(tape.oldest().map(|e| e.epoch), Some(3));
        assert_eq!(tape.newest().map(|e| e.epoch), Some(5));
        assert!(tape.at(2).is_some());
    }

    #[test]
    fn a_zero_sized_tape_records_nothing() {
        let mut tape: FlightTape<0> = FlightTape::new();
        tape.record_code(1, 2, 3);
        assert!(tape.is_empty());
        assert!(!tape.is_full(), "N = 0 is empty, never full");
        assert_eq!(tape.iter().count(), 0);
        assert!(tape.newest().is_none());
        assert!(tape.oldest().is_none());
        assert!(tape.at(0).is_none());
        assert!(tape.last_with_code(3).is_none());
    }

    #[test]
    fn the_newest_matching_code_wins() {
        let mut tape: FlightTape<4> = FlightTape::new();
        tape.record_code(1, 1, 7);
        tape.record_code(1, 2, 9);
        tape.record_code(2, 3, 7);
        tape.record_code(2, 4, 5);

        let hit = tape.last_with_code(7);
        assert_eq!(
            hit.map(|e| (e.epoch, e.task_id)),
            Some((2, 3)),
            "newest first: the older epoch-1 event must not win"
        );
        assert!(tape.last_with_code(42).is_none());
    }

    #[test]
    fn clear_rewinds_the_cursor_as_well_as_the_cells() {
        let mut tape: FlightTape<2> = FlightTape::new();
        tape.record_code(1, 1, 1);
        tape.record_code(1, 2, 2);
        tape.record_code(1, 3, 3);
        tape.clear();
        assert!(tape.is_empty());
        assert!(tape.newest().is_none());

        tape.record_code(9, 9, 9);
        assert!(tape.iter().map(|e| e.code).eq([9u32]));
        assert_eq!(tape.len(), 1, "a cleared tape starts over, not half-full");
    }
}
