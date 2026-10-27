//! Core wire parsing for the supervised-worker protocol (`SNUB` frames).
//!
//! Pure `core`: no allocator, no threads, no wall clock. This module is
//! always compiled, so embedded and async callers can drive the SAME
//! parser the std supervisor uses — push chunks through
//! [`SnubScanner::push`] and receive fully parsed [`ProcessToken`]s.
//! The std-flavored drivers (reader thread, blocking EOF pump, poison-
//! recovering locks) live in [`crate::supervisor::PipeDrain`] and sit on
//! top of [`SnubScanner`].
//!
//! Wire format: `SNUB` magic ([`TOKEN_MAGIC`]) + 40 B body
//! (8 B task + 4 B opcode + [`TOKEN_PAYLOAD_LEN`] B payload).

/// Payload bytes carried by one [`ProcessToken`] frame.
///
/// FORMAT CONTRACT: 28 B on the wire (`u64 task + u32 opcode + 28 B
/// payload` = 40 B body after [`TOKEN_MAGIC`]). The state handoff carries
/// 24 digest bytes per frame (two 24 B chunks, then 16 B: 24/24/16) against
/// this ceiling.
pub const TOKEN_PAYLOAD_LEN: usize = 28;

/// One token body: payload plus `task`/`opcode` fields, bytes.
pub const TOKEN_BODY_LEN: usize = core::mem::size_of::<ProcessToken>();

const _: () = assert!(
    TOKEN_PAYLOAD_LEN == 28,
    "token payload is a 28 B format contract"
);
const _: () = assert!(TOKEN_BODY_LEN == 40, "token body is a 40 B format contract");

/// Last-armed 40-byte token: `[task u64 | opcode u32 | payload [u8;28]]`.
///
/// `OP_COMMIT` clears the armed token (settled clean). Any other opcode
/// arms it until commit or `EOF`. Fixed bytes only — codes + ids, no
/// paths. `Copy`, no alloc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessToken {
    /// Correlation id (`task_id` of the `TransactionHeader`).
    pub task: u64,
    /// Action code (`OP_COMMIT` = settle-clean, else emergency opcode).
    pub opcode: u32,
    /// Fixed payload (file id, relay index — never a path).
    pub payload: [u8; TOKEN_PAYLOAD_LEN],
}

/// Framing sync-word prefix ("SNUB" = 0x53, 0x4E, 0x55, 0x42).
/// Prevents unaligned byte drift if arbitrary text (println!) is sent through stdout.
pub const TOKEN_MAGIC: [u8; 4] = *b"SNUB";

/// Settle-clean marker: clears `last_armed_token` in the drain.
pub const OP_COMMIT: u32 = 0;

/// Idle heartbeat: worker sends `[task=0, OP_HEARTBEAT, zero]` every
/// `lease_ms / 2` while idle (no transactions flowing). Updates
/// `last_seen` like any traffic, arms nothing. Traffic-is-liveness:
/// any 40 B frame (token/commit/heartbeat) proves the worker pumps.
pub const OP_HEARTBEAT: u32 = 1;

/// Cold snapshot marker: worker flushed a 512 B `SnapshotV1` out of band
/// (file, sidecar pipe, crash dump). The supervisor treats it as
/// diagnostics, never as an armed token — arms nothing, clears nothing.
pub const OP_SNAPSHOT: u32 = 2;

/// Hot state handoff (Phase 3): the 64 B `StateDigest` travels as THREE
/// `OP_STATE` frames (seq in `task`: 0 = bytes `0..24`, 1 = `24..48`,
/// 2 = `48..64` + full-digest CRC), each with its own frame CRC. The
/// drain keeps the LAST valid digest in its own slot — never armed as a
/// token, never rolled back. The supervisor hands it to the next
/// generation on tick 0 (`take_state()`).
pub const OP_STATE: u32 = 3;

// COMPILE-TIME GATES (token wire math; core-safe, so no_std builds see
// them too):
// * token BODY = 8 (task) + 4 (opcode) + 28 (payload) = 40 B;
// * token WIRE = 40 B body + 4 B `SNUB` magic = 44 B (NOT 44 in-struct!).
const _: () = {
    assert!(
        core::mem::size_of::<ProcessToken>() == 40,
        "CRITICAL: ProcessToken body must be exactly 40 bytes (8+4+28)"
    );
    // PORTABILITY: the body is proven pointer-width-independent (8+4+28
    // of FIXED primitives), not merely pinned at 40. No `usize` may ever
    // enter a wire struct — `u32/u64/u8` only, on any target.
    assert!(
        core::mem::size_of::<ProcessToken>()
            == core::mem::size_of::<u64>() + core::mem::size_of::<u32>() + 28,
        "PORTABILITY: ProcessToken size must not depend on pointer width"
    );
    assert!(
        core::mem::size_of::<ProcessToken>() + TOKEN_MAGIC.len() == 44,
        "CRITICAL: wire frame must be exactly 44 bytes (40 body + 4 SNUB)"
    );
};

impl ProcessToken {
    /// Build from parts (worker side, no alloc).
    #[inline]
    #[must_use]
    pub const fn new(task: u64, opcode: u32, payload: [u8; TOKEN_PAYLOAD_LEN]) -> Self {
        Self {
            task,
            opcode,
            payload,
        }
    }

    /// Settle-clean marker for `task`.
    #[inline]
    #[must_use]
    pub const fn commit(task: u64) -> Self {
        Self {
            task,
            opcode: OP_COMMIT,
            payload: [0; 28],
        }
    }

    /// Ergonomic constructor for single-byte tag payload.
    #[inline]
    #[must_use]
    pub const fn simple(task: u64, opcode: u32, tag: u8) -> Self {
        let mut payload = [0u8; 28];
        payload[0] = tag;
        Self {
            task,
            opcode,
            payload,
        }
    }

    /// Ergonomic constructor for single-byte payload when task ID is stamped automatically by the node.
    #[inline]
    #[must_use]
    pub const fn tagged(opcode: u32, tag: u8) -> Self {
        Self::simple(0, opcode, tag)
    }

    /// Serialize to 44 wire bytes with "SNUB" sync magic (little-endian, no alloc).
    #[inline]
    #[must_use]
    pub fn to_bytes(self) -> [u8; 44] {
        let mut out = [0u8; 44];
        out[0..4].copy_from_slice(&TOKEN_MAGIC);
        out[4..12].copy_from_slice(&self.task.to_le_bytes());
        out[12..16].copy_from_slice(&self.opcode.to_le_bytes());
        out[16..44].copy_from_slice(&self.payload);
        out
    }

    /// Parse 40 body wire bytes (excluding the 4-byte magic).
    #[inline]
    #[must_use]
    pub fn from_body_bytes(raw: &[u8; 40]) -> Self {
        let mut task = [0u8; 8];
        let mut opcode = [0u8; 4];
        let mut payload = [0u8; 28];
        task.copy_from_slice(&raw[0..8]);
        opcode.copy_from_slice(&raw[8..12]);
        payload.copy_from_slice(&raw[12..40]);
        Self {
            task: u64::from_le_bytes(task),
            opcode: u32::from_le_bytes(opcode),
            payload,
        }
    }

    /// Parse the 40 body bytes of a token (the `SNUB` magic and CRC, if
    /// present, must be stripped first — see [`Self::to_bytes`] and
    /// [`Self::from_body_bytes`]).
    #[inline]
    #[must_use]
    pub fn from_bytes(raw: &[u8; 40]) -> Self {
        Self::from_body_bytes(raw)
    }
}

/// Incremental `SNUB` frame scanner: survives arbitrary chunk boundaries.
///
/// No alloc, no clock, no locks — the pure parse core behind
/// [`crate::supervisor::PipeDrain`]. Feed raw bytes from any source
/// (UART/DMA buffer, socket, pipe); every complete 44 B frame is
/// delivered through `on_token`, garbage between frames is skipped, and
/// a torn partial frame stays buffered until its continuation arrives
/// (or is dropped by [`SnubScanner::reset`]).
#[derive(Debug)]
pub struct SnubScanner {
    window: [u8; 4],
    body: [u8; 40],
    body_len: usize,
    in_body: bool,
}

impl Default for SnubScanner {
    fn default() -> Self {
        Self {
            window: [0u8; 4],
            body: [0u8; 40],
            body_len: 0,
            in_body: false,
        }
    }
}

impl SnubScanner {
    /// Consume a chunk, calling `on_token` for every complete frame.
    /// Returns the number of frames delivered. A partial frame stays in
    /// the scanner until the next chunk (or is dropped at [`SnubScanner::reset`]).
    pub fn push<F: FnMut(ProcessToken)>(&mut self, mut chunk: &[u8], mut on_token: F) -> usize {
        let mut frames = 0;
        while !chunk.is_empty() {
            if self.in_body {
                let take = (40 - self.body_len).min(chunk.len());
                self.body[self.body_len..self.body_len + take].copy_from_slice(&chunk[..take]);
                self.body_len += take;
                chunk = &chunk[take..];
                if self.body_len == 40 {
                    let token = ProcessToken::from_body_bytes(&self.body);
                    // Reset BEFORE delivering: a panicking `on_token` caught
                    // upstream must not leave `body_len == 40`, or the next
                    // push re-enters this branch with `take == 0` and
                    // delivers the same frame twice.
                    self.reset();
                    frames += 1;
                    on_token(token);
                }
            } else {
                self.window[0] = self.window[1];
                self.window[1] = self.window[2];
                self.window[2] = self.window[3];
                self.window[3] = chunk[0];
                chunk = &chunk[1..];
                if self.window == TOKEN_MAGIC {
                    self.in_body = true;
                    self.body_len = 0;
                }
            }
        }
        frames
    }

    /// Drop any partial frame and re-arm the magic scan.
    pub fn reset(&mut self) {
        self.window = [0u8; 4];
        self.body = [0u8; 40];
        self.body_len = 0;
        self.in_body = false;
    }
}
