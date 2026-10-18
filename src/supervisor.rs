//! Lifecycle supervisor: dual-path process monitoring, restart tracking, and token recovery.
//!
//! PATH 1: Process exit / hang monitoring. Reaps the child process, emits
//! `SnubberEvent::Aborted { reason: "process died" }`, and triggers policy-based restart
//! at `generation + 1`. [`EpochGate`](crate::EpochGate) rejects stale generation packets.
//!
//! PATH 2: Emergency token recovery. The worker maintains an armed 40-byte token over an
//! anonymous pipe. The supervisor background reader thread drains the pipe in real time.
//! On clean commit (`OP_COMMIT`), the armed token is cleared. On process abort (`EOF` while
//! armed), the un-settled token executes emergency rollback on the supervisor side.
//!
//! PIPE-CHOKE CONTRACT: Drain the pipe in a background reader thread in real time
//! (OS kernel buffer is 4-64 KB; un-drained stdout stalls the child on `write()`).
//! Drain retains at most ONE last-armed token, cleared on `OP_COMMIT`.
//!
//! RESTART LOGIC: The caller owns process spawn and wait. The supervisor provides
//! decision math via [`RestartPolicy`], [`Backoff`], and [`RestartTracker`].
//!
//! ORPHAN CONTRACT: Defines the guarantee that child processes terminate with the parent.
//! Portable no-op included ([`NoOrphanGuard`]); OS-level implementation is application-owned.

use super::{SnubberEvent, TransactionHeader};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Monotonic generation clock for ONE supervised lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenerationClock {
    generation: u64,
    next_epoch: u64,
}

impl GenerationClock {
    /// Boot lineage: generation 1, epoch 1 (gen 0 = un-supervised past).
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            generation: 1,
            next_epoch: 1,
        }
    }

    /// Current generation (handed to the worker as `SNUBBER_GEN`).
    #[inline]
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Next epoch in this generation (monotonic, never 0).
    #[inline]
    pub fn alloc_epoch(&mut self) -> u64 {
        let e = self.next_epoch;
        self.next_epoch = self.next_epoch.saturating_add(1).max(1);
        e.max(1)
    }

    /// Death observed: bump generation, reset epoch (re-handshake).
    #[inline]
    pub fn restart(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1).max(1);
        self.next_epoch = 1;
        self.generation
    }
}

impl Default for GenerationClock {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Why the worker died (`Copy` only — no handles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeathCause {
    /// Pipe broke / handle signaled (`abort()`, panic-abort, SIGKILL).
    AbortUnwind,
    /// No traffic past the lease.
    HangTimeout,
    /// Reaped from outside (operator, OOM-killer, parent).
    KilledOutside,
}

impl DeathCause {
    /// Static reason stamped into the `Aborted` event.
    #[inline]
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::AbortUnwind => "process died",
            Self::HangTimeout => "process hung past lease",
            Self::KilledOutside => "process reaped from outside",
        }
    }
}

/// When the supervisor restarts an exited worker process (Phase 2).
///
/// Plain `Copy` data — no logic lives here. The [`RestartTracker`]
/// interprets it against the storm window:
/// * `Never` — one-shot tasks (export, migration): no respawn.
/// * `OnCrash` — clean `exit(0)` is a graceful shutdown (no restart);
///   any other termination (`exit(1)`, segfault, `TerminateProcess`,
///   `HangTimeout`) is a crash that triggers a restart.
/// * `Always` — daemons that must run continuously: even `exit(0)`
///   is respawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RestartPolicy {
    /// Never re-spawn (one-shot tasks).
    Never,
    /// Re-spawn on crash only (`exit_code != 0` or hang/kill).
    #[default]
    OnCrash,
    /// Re-spawn on every death, including clean `exit(0)`.
    Always,
}

impl RestartPolicy {
    /// `true` if this death deserves a restart attempt.
    /// `exit_code: None` = killed/hung from outside (always a crash).
    #[inline]
    #[must_use]
    pub const fn wants_restart(self, exit_code: Option<i32>) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::OnCrash => !matches!(exit_code, Some(0)),
        }
    }
}

/// Pause between process restarts (Phase 2).
///
/// `Copy`, `const`-friendly. [`Backoff::delay_for_attempt`] is pure
/// math (`saturating` shifts, no clock, no sleep): attempt 0 waits
/// `base_ms`, each next attempt doubles, capped at `max_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Backoff {
    /// First pause, ms. `0` = no cooling (explicit hammer).
    pub base_ms: u64,
    /// Ceiling for the exponential growth, ms.
    pub max_ms: u64,
}

impl Backoff {
    /// No cooling: re-spawn immediately (explicit, never default).
    #[inline]
    #[must_use]
    pub const fn none() -> Self {
        Self {
            base_ms: 0,
            max_ms: 0,
        }
    }

    /// Exponential cooling: `base_ms, 2*base, 4*base, …`, capped.
    #[inline]
    #[must_use]
    pub const fn exponential(base_ms: u64, max_ms: u64) -> Self {
        Self {
            base_ms,
            max_ms: if max_ms < base_ms { base_ms } else { max_ms },
        }
    }

    /// Pause before restart attempt `n` (0-based), ms.
    /// Pure saturating math: `base << n`, capped at `max_ms`.
    /// `n >= 63` saturates (never panics, MSRV 1.70-safe loop).
    #[inline]
    #[must_use]
    pub const fn delay_for_attempt(self, attempt: u32) -> u64 {
        if self.base_ms == 0 {
            return 0;
        }
        let mut grown = self.base_ms;
        let mut i = 0u32;
        while i < attempt {
            grown = grown.saturating_mul(2);
            if grown >= self.max_ms {
                return self.max_ms;
            }
            i += 1;
        }
        if grown > self.max_ms {
            self.max_ms
        } else {
            grown
        }
    }
}

impl Default for Backoff {
    #[inline]
    fn default() -> Self {
        Self::exponential(50, 1000)
    }
}

/// Storm-guard window: at most `max` restarts per `within_ms` (Phase 2).
///
/// `Copy`. `max = 0` = guard open (restart every crash — explicit, for
/// daemons behind an outer supervisor). The [`RestartTracker`] holds
/// the timestamp deque; this struct is the declarative half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RestartWindow {
    /// Max restarts inside the window (`0` = unlimited).
    pub max_restarts: u32,
    /// Window length, ms.
    pub within_ms: u64,
}

impl RestartWindow {
    /// `max` restarts per `within_secs` seconds. Saturates, never panics.
    #[inline]
    #[must_use]
    pub const fn new(max_restarts: u32, within_secs: u64) -> Self {
        Self {
            max_restarts,
            within_ms: within_secs.saturating_mul(1000),
        }
    }

    /// Unlimited restarts (explicit — outer supervisor owns the guard).
    #[inline]
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_restarts: 0,
            within_ms: 0,
        }
    }
}

impl Default for RestartWindow {
    #[inline]
    fn default() -> Self {
        Self::new(3, 10)
    }
}

/// What the tracker decided after one death report (Phase 2).
///
/// The caller owns sleep + spawn; the crate owns the math. `delay_ms`
/// is ADVICE the caller sleeps with its own sleeper.
#[must_use = "a RestartDecision orders the lineage (Stop/GiveUp/Restart) — match it, never drop it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartDecision {
    /// Do not re-spawn: `Never`, clean exit under `OnCrash`, or storm.
    Stop,
    /// Storm guard tripped: caller MUST NOT spawn — `report(ProcessDied)`
    /// + `RollbackAndAlert` on its node, park the lineage.
    GiveUp {
        /// Deaths counted inside the window (always `>= max`).
        deaths_in_window: u32,
    },
    /// Re-spawn after `delay_ms` (caller sleeps with its own sleeper).
    Restart {
        /// Caller-owned cooling pause, ms (`0` = immediate).
        delay_ms: u64,
        /// Which restart attempt this is (0-based, for logging).
        attempt: u32,
    },
}

/// Restart rate accountant (Phase 2).
///
/// Owns: storm-window timestamps (`VecDeque<u64>`, pruned per report),
/// attempt counter, generation clock. Owns NOT: spawning, sleeping,
/// pipes, threads. Caller loop:
///
/// ```text
/// loop {
///     let status = spawn_and_wait();          // caller owns
///     match tracker.note_death(exit_code, now_ms()) {
///         Stop => break,
///         GiveUp { .. } => { node.report(ProcessDied,…); break; }
///         Restart { delay_ms, .. } => { caller_sleep(delay_ms); }
///     }
/// }
/// ```
///
/// `now_ms` is caller-supplied (tests inject a fake clock — zero real
/// sleeps). Timestamps need only be monotonic, never wall-clock.
#[derive(Debug, Clone)]
pub struct RestartTracker {
    policy: RestartPolicy,
    backoff: Backoff,
    window: RestartWindow,
    deaths_ms: VecDeque<u64>,
    attempts: u32,
    restarts: u32,
    giveups: u32,
    clock: GenerationClock,
}

impl RestartTracker {
    /// Build from declarative parts. The deque is preallocated at
    /// `max_restarts.max(1)` and pruned — never grown past `max+1`.
    #[must_use]
    pub fn new(policy: RestartPolicy, backoff: Backoff, window: RestartWindow) -> Self {
        let cap = window.max_restarts.max(1) as usize;
        Self {
            policy,
            backoff,
            window,
            deaths_ms: VecDeque::with_capacity(cap),
            attempts: 0,
            restarts: 0,
            giveups: 0,
            clock: GenerationClock::new(),
        }
    }

    /// Default restart tracker configuration: `OnCrash`, `50ms→1s` backoff, `3 per 10s` storm guard.
    #[must_use]
    pub fn default_tracker() -> Self {
        Self::new(
            RestartPolicy::OnCrash,
            Backoff::default(),
            RestartWindow::default(),
        )
    }

    /// Backwards-compatible alias for [`Self::default_tracker`].
    #[must_use]
    #[inline]
    pub fn default_spine() -> Self {
        Self::default_tracker()
    }
}

impl Default for RestartTracker {
    fn default() -> Self {
        Self::default_tracker()
    }
}

impl RestartTracker {
    /// Report one death at `now_ms` (caller clock, monotonic).
    /// `exit_code: None` = killed/hung from outside (always a crash).
    /// Returns the decision; the CALLER sleeps/spawns/reports.
    pub fn note_death(&mut self, exit_code: Option<i32>, now_ms: u64) -> RestartDecision {
        self.attempts = self.attempts.saturating_add(1);
        if !self.policy.wants_restart(exit_code) {
            return RestartDecision::Stop;
        }
        if self.window.max_restarts > 0 {
            let cutoff = now_ms.saturating_sub(self.window.within_ms);
            while self.deaths_ms.front().is_some_and(|&t| t < cutoff) {
                self.deaths_ms.pop_front();
            }
            self.deaths_ms.push_back(now_ms);
            while self.deaths_ms.len() > self.window.max_restarts as usize + 1 {
                self.deaths_ms.pop_front();
            }
            if self.deaths_ms.len() > self.window.max_restarts as usize {
                self.giveups = self.giveups.saturating_add(1);
                return RestartDecision::GiveUp {
                    deaths_in_window: self.deaths_ms.len() as u32,
                };
            }
        }
        let attempt = self.restarts;
        let delay_ms = self.backoff.delay_for_attempt(attempt);
        self.restarts = self.restarts.saturating_add(1);
        self.clock.restart();
        RestartDecision::Restart { delay_ms, attempt }
    }

    /// Total death reports seen (restarts + stops + giveups).
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Restarts authorized so far.
    #[must_use]
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Storm-guard trips so far.
    #[must_use]
    pub fn giveups(&self) -> u32 {
        self.giveups
    }

    /// Current supervised generation (bumped per authorized restart).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.clock.generation()
    }
}

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
    pub payload: [u8; 28],
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

/// Hot state handoff (Phase 3): the 40 B body carries a chunk of the
/// 64 B `StateDigest` (2 frames: seq 0 = bytes `0..28` + CRC, seq 1 =
/// bytes `28..64`). The drain keeps the LAST valid digest in its own
/// slot — never armed as a token, never rolled back. The supervisor
/// hands it to the next generation on tick 0 (`take_state()`).
pub const OP_STATE: u32 = 3;

impl ProcessToken {
    /// Build from parts (worker side, no alloc).
    #[inline]
    #[must_use]
    pub const fn new(task: u64, opcode: u32, payload: [u8; 28]) -> Self {
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

    /// Check if running under a supervisor (`SNUBBER_SUPERVISED` env var is set).
    /// Cached in `OnceLock` on first check — zero syscalls on hot paths.
    #[inline]
    pub fn is_supervised() -> bool {
        use std::sync::OnceLock;
        static SUPERVISED: OnceLock<bool> = OnceLock::new();
        *SUPERVISED.get_or_init(|| std::env::var_os("SNUBBER_SUPERVISED").is_some())
    }

    /// Emit framed token to stdout if running under a supervisor (`SNUBBER_SUPERVISED` env var is set).
    /// Token is preceded by the 4-byte `SNUB` sync magic, making it immune to stray text in stdout.
    pub fn emit_if_supervised(&self) {
        if Self::is_supervised() {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = lock.write_all(&self.to_bytes());
            let _ = lock.flush();
        }
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

    /// Parse wire bytes. Accepts either a 44-byte framed packet (with `SNUB` magic)
    /// or a raw legacy 40-byte packet.
    #[inline]
    #[must_use]
    pub fn from_bytes(raw: &[u8; 40]) -> Self {
        Self::from_body_bytes(raw)
    }
}

/// State digest payload: exactly 64 bytes (one CPU cache line).
pub const STATE_LEN: usize = 64;

// COMPILE-TIME GATES (hot-handoff wire math; this module is `std`-gated,
// so `no_std` builds never see these — `cargo check --no-default-features`
// stays green):
// * state snapshot = exactly one L1 cache line (64 B);
// * token BODY = 8 (task) + 4 (opcode) + 28 (payload) = 40 B;
// * token WIRE = 40 B body + 4 B `SNUB` magic = 44 B (NOT 44 in-struct!);
// * digest split = 24 + 24 + 16 chunks = 64 B (`to_frames` arithmetic);
// * opcodes pairwise distinct (drain dispatch never confuses a chunk).
const _: () = {
    assert!(
        STATE_LEN == 64,
        "CRITICAL: STATE_LEN must be exactly one 64-byte cache line"
    );
    assert!(
        core::mem::size_of::<StateDigest>() == 64,
        "CRITICAL: StateDigest must be exactly 64 bytes (one cache line)"
    );
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
    assert!(
        24 + 24 + 16 == STATE_LEN,
        "CRITICAL: OP_STATE chunk split must cover the whole digest"
    );
    assert!(
        OP_COMMIT != OP_HEARTBEAT
            && OP_COMMIT != OP_SNAPSHOT
            && OP_COMMIT != OP_STATE
            && OP_HEARTBEAT != OP_SNAPSHOT
            && OP_HEARTBEAT != OP_STATE
            && OP_SNAPSHOT != OP_STATE,
        "CRITICAL: supervisor opcodes must be pairwise distinct"
    );
    // LE WIRE CANON (universal, platform-blind): every multi-byte integer
    // on the wire is explicit little-endian (`to_le_bytes` /
    // `from_le_bytes` ONLY). No `to_be` / `to_ne`, no `transmute`, no
    // pointer casts on the wire path — a big-endian peer decodes the same
    // bytes identically, because the bytes never depend on host order.
    // (`to_le_bytes` is `const` but `==` on arrays is not const-stable on
    // MSRV 1.70, so the full-vector proof lives in the `wire_is_le_canon`
    // unit test below; here we pin the scalar first byte, which IS const.)
    assert!(
        0x12345678u32.to_le_bytes()[0] == 0x78,
        "WIRE PROTOCOL: explicit little-endian encoding broken"
    );
};

/// Hot state digest (Phase 3): 64-byte process state snapshot.
///
/// Fixed 64 B of OPAQUE bytes + CRC32 — one cache line, ~1 ns copy.
/// The crate never interprets the payload: user-defined flat-packed coordinates
/// or checkpoint references (checkpoint id + file offset + hash) for states on disk.
/// Framed pipe transport guarantees encoding, CRC integrity verification, and in-order delivery.
///
/// Wire format: THREE `OP_STATE` frames (`task` = seq 0/1/2).
/// Frame 0 & 1 carry 24 bytes of data + 4 bytes of chunk CRC.
/// Frame 2 carries 16 bytes of data + 4 bytes of full-digest CRC + 4 bytes padding + 4 bytes chunk CRC.
/// Reassembly verifies individual chunk CRCs and validates the full 64-byte payload against the full-digest CRC.
/// Torn or mismatched frames dropped — replacement process receives the last verified complete
/// state digest, never a corrupt or fragmented hybrid state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateDigest {
    /// Opaque state bytes (user-packed, user-read).
    pub bytes: [u8; STATE_LEN],
}

impl Default for StateDigest {
    #[inline]
    fn default() -> Self {
        Self {
            bytes: [0; STATE_LEN],
        }
    }
}

/// CRC32-IEEE over digest bytes (same table as `SnapshotV1`).
fn state_crc(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data.iter() {
        crc ^= b as u32;
        let mut i = 0;
        while i < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            i += 1;
        }
    }
    !crc
}

impl StateDigest {
    /// Wrap raw bytes (user-packed flat struct or checkpoint reference).
    #[inline]
    #[must_use]
    pub const fn new(bytes: [u8; STATE_LEN]) -> Self {
        Self { bytes }
    }

    /// CRC32 of the full 64 B payload (integrity seal).
    #[inline]
    #[must_use]
    pub fn crc(&self) -> u32 {
        state_crc(&self.bytes)
    }

    /// Split into three `OP_STATE` wire frames (seq in `task`).
    /// Body layout per frame: `[chunk | crc u32]` where chunk =
    /// 24/24/16 digest bytes at `payload[0..take]`. Frame 2 embeds the full-digest
    /// CRC32 at `payload[16..20]`. Frame CRC at `payload[24..28]` covers the chunk
    /// payload (frames 0/1 cover 24 B; frame 2 covers 16 B data + 4 B full CRC = 20 B).
    /// Torn or corrupted frames drop silently.
    #[must_use]
    pub fn to_frames(self) -> [ProcessToken; 3] {
        let full_crc = self.crc();
        let mut frames = [ProcessToken::tagged(OP_STATE, 0); 3];
        let mut off = 0usize;
        for (seq, frame) in frames.iter_mut().enumerate() {
            let take = if seq < 2 { 24 } else { 16 };
            let mut payload = [0u8; 28];
            payload[0..take].copy_from_slice(&self.bytes[off..off + take]);
            if seq == 2 {
                payload[16..20].copy_from_slice(&full_crc.to_le_bytes());
                let crc = state_crc(&payload[0..20]);
                payload[24..28].copy_from_slice(&crc.to_le_bytes());
            } else {
                let crc = state_crc(&self.bytes[off..off + take]);
                payload[24..28].copy_from_slice(&crc.to_le_bytes());
            }
            *frame = ProcessToken {
                task: seq as u64,
                opcode: OP_STATE,
                payload,
            };
            off += take;
        }
        frames
    }

    /// Publish to stdout under a supervisor (satellite side).
    /// No-op when unsupervised. Flushes — drain reads in real time.
    pub fn emit_if_supervised(&self) {
        if ProcessToken::is_supervised() {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            for f in self.to_frames() {
                let _ = lock.write_all(&f.to_bytes());
            }
            let _ = lock.flush();
        }
    }
}

/// Build the `Aborted` death event (Path-1 emit).
/// v0.4 honest: death is `ProcessDied`, never masked (D3 closed).
/// Return is `SnubberEvent` (already `#[must_use]` at the type).
#[inline]
pub fn death_event(header: TransactionHeader, cause: DeathCause) -> SnubberEvent<()> {
    SnubberEvent::Aborted {
        header,
        reason: cause.reason(),
    }
}

/// Drain state: exactly ONE slot (last un-settled token) + liveness clock
/// + hot handoff (last verified complete `StateDigest` + 3-frame reassembly scratch).
///
/// `last_seen_ms`: wall-clock of the last valid 40 B frame (token,
/// commit, heartbeat OR state — any traffic proves the worker pumps).
/// `0` = nothing seen yet.
#[derive(Debug, Default)]
struct DrainState {
    armed: Option<ProcessToken>,
    last_seen_ms: u64,
    /// Last verified complete StateDigest.
    state: Option<StateDigest>,
    /// Reassembly scratch: chunks per seq (24/24/16) + got-mask + full CRC.
    scratch: [[u8; 28]; 3],
    scratch_got: u8,
    scratch_full_crc: u32,
}

/// Monotonic millisecond clock (supervisor side, `std`).
#[inline]
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pipe drain: background reader keeping the kernel buffer empty.
///
/// Robust framing: scans for `TOKEN_MAGIC` ("SNUB"). Any non-token output
/// (e.g. `println!` logs) in stdout is transparently skipped without losing
/// frame alignment or crashing.
#[derive(Debug)]
pub struct PipeDrain {
    state: Arc<Mutex<DrainState>>,
    done: Arc<AtomicU64>,
}

impl PipeDrain {
    /// Spawn the drain on `reader`. Returns immediately, thread owns it.
    #[must_use]
    pub fn spawn<R: Read + Send + 'static>(reader: R) -> Self {
        let state = Arc::new(Mutex::new(DrainState::default()));
        let done = Arc::new(AtomicU64::new(0));
        let s = Arc::clone(&state);
        let d = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::with_capacity(4096, reader);
            let mut window = [0u8; 4];
            let mut one = [0u8; 1];

            while let Ok(()) = reader.read_exact(&mut one) {
                window[0] = window[1];
                window[1] = window[2];
                window[2] = window[3];
                window[3] = one[0];

                if window == TOKEN_MAGIC {
                    let mut body = [0u8; 40];
                    if reader.read_exact(&mut body).is_err() {
                        break;
                    }
                    let token = ProcessToken::from_body_bytes(&body);
                    if let Ok(mut st) = s.lock() {
                        st.last_seen_ms = now_ms();
                        if token.opcode == OP_COMMIT || token.opcode == OP_HEARTBEAT {
                            if token.opcode == OP_COMMIT {
                                st.armed = None;
                            }
                        } else if token.opcode == OP_STATE {
                            // Hot handoff chunk: seq = task (0/1/2).
                            // Layout: payload[0..take] = data (24/24/16).
                            // Frame 2 carries authoritative full-digest CRC at payload[16..20].
                            // Frame CRC at payload[24..28].
                            let seq = (token.task.min(2)) as usize;
                            let take = if seq < 2 { 24 } else { 16 };
                            let mut want = [0u8; 4];
                            want.copy_from_slice(&token.payload[24..28]);
                            let want = u32::from_le_bytes(want);

                            let valid_chunk = if seq == 2 {
                                state_crc(&token.payload[0..20]) == want
                            } else {
                                state_crc(&token.payload[0..take]) == want
                            };

                            if valid_chunk {
                                st.scratch[seq][0..take].copy_from_slice(&token.payload[0..take]);
                                if seq == 2 {
                                    let mut fcrc = [0u8; 4];
                                    fcrc.copy_from_slice(&token.payload[16..20]);
                                    st.scratch_full_crc = u32::from_le_bytes(fcrc);
                                }
                                st.scratch_got |= 1 << seq;
                                if st.scratch_got == 0b111 {
                                    let mut bytes = [0u8; STATE_LEN];
                                    bytes[0..24].copy_from_slice(&st.scratch[0][0..24]);
                                    bytes[24..48].copy_from_slice(&st.scratch[1][0..24]);
                                    bytes[48..64].copy_from_slice(&st.scratch[2][0..16]);
                                    let candidate = StateDigest::new(bytes);
                                    if candidate.crc() == st.scratch_full_crc {
                                        st.state = Some(candidate);
                                    }
                                    // Reset mask so subsequent digests reassemble cleanly
                                    st.scratch_got = 0;
                                }
                            }
                            // Corrupt or mismatched frame: dropped silently,
                            // replacement process gets the last verified whole state.
                        } else {
                            st.armed = Some(token);
                        }
                    }
                    window = [0u8; 4];
                }
            }
            d.store(1, Ordering::SeqCst);
        });
        Self { state, done }
    }

    /// Synchronously process a reader to EOF without spawning a background OS thread.
    /// Ideal for in-memory byte buffers, fuzzing sweeps, and synchronous parsers.
    #[must_use]
    pub fn drain_sync<R: Read>(reader: R) -> Self {
        let state = Arc::new(Mutex::new(DrainState::default()));
        let done = Arc::new(AtomicU64::new(0));
        let mut reader = std::io::BufReader::with_capacity(4096, reader);
        let mut window = [0u8; 4];
        let mut one = [0u8; 1];

        while let Ok(()) = reader.read_exact(&mut one) {
            window[0] = window[1];
            window[1] = window[2];
            window[2] = window[3];
            window[3] = one[0];

            if window == TOKEN_MAGIC {
                let mut body = [0u8; 40];
                if reader.read_exact(&mut body).is_err() {
                    break;
                }
                let token = ProcessToken::from_body_bytes(&body);
                if let Ok(mut st) = state.lock() {
                    st.last_seen_ms = now_ms();
                    if token.opcode == OP_COMMIT || token.opcode == OP_HEARTBEAT {
                        if token.opcode == OP_COMMIT {
                            st.armed = None;
                        }
                    } else if token.opcode == OP_STATE {
                        let seq = (token.task.min(2)) as usize;
                        let take = if seq < 2 { 24 } else { 16 };
                        let mut want = [0u8; 4];
                        want.copy_from_slice(&token.payload[24..28]);
                        let want = u32::from_le_bytes(want);

                        let valid_chunk = if seq == 2 {
                            state_crc(&token.payload[0..20]) == want
                        } else {
                            state_crc(&token.payload[0..take]) == want
                        };

                        if valid_chunk {
                            st.scratch[seq][0..take].copy_from_slice(&token.payload[0..take]);
                            if seq == 2 {
                                let mut fcrc = [0u8; 4];
                                fcrc.copy_from_slice(&token.payload[16..20]);
                                st.scratch_full_crc = u32::from_le_bytes(fcrc);
                            }
                            st.scratch_got |= 1 << seq;
                            if st.scratch_got == 0b111 {
                                let mut bytes = [0u8; STATE_LEN];
                                bytes[0..24].copy_from_slice(&st.scratch[0][0..24]);
                                bytes[24..48].copy_from_slice(&st.scratch[1][0..24]);
                                bytes[48..64].copy_from_slice(&st.scratch[2][0..16]);
                                let candidate = StateDigest::new(bytes);
                                if candidate.crc() == st.scratch_full_crc {
                                    st.state = Some(candidate);
                                }
                                st.scratch_got = 0;
                            }
                        }
                    } else {
                        st.armed = Some(token);
                    }
                }
                window = [0u8; 4];
            }
        }
        done.store(1, Ordering::SeqCst);
        Self { state, done }
    }

    /// Take the last un-settled token, if any (call AFTER `EOF`/reap).
    #[must_use]
    pub fn take_armed(&self) -> Option<ProcessToken> {
        self.state.lock().ok().and_then(|mut st| st.armed.take())
    }

    /// Take the last verified complete `StateDigest`, if any (Phase 3).
    /// Call after process exit, hand the bytes to the replacement process at startup (child
    /// stdin/env/file — the crate delivers, the app injects). `Copy`
    /// out under one short lock — no alloc, never panics.
    #[must_use]
    pub fn take_state(&self) -> Option<StateDigest> {
        self.state.lock().ok().and_then(|st| st.state)
    }

    /// Peek without taking (supervisor polls before spawning replacement process).
    #[must_use]
    pub fn last_state(&self) -> Option<StateDigest> {
        self.state.lock().ok().and_then(|st| st.state)
    }

    /// `true` once the reader hit `EOF`/error.
    #[must_use]
    pub fn eof(&self) -> bool {
        self.done.load(Ordering::SeqCst) != 0
    }

    /// Milliseconds since the last valid frame (`u64::MAX` if never).
    /// The supervisor turns `elapsed > lease_ms` (pipe alive, no traffic)
    /// into `DeathCause::HangTimeout`: `take_armed()` → `child.kill()`
    /// (`TerminateProcess`/SIGKILL from OUTSIDE) → `clock.restart()`.
    /// A reaped-then-hung PID is abandoned — the generation already moved.
    #[must_use]
    pub fn idle_ms(&self) -> u64 {
        match self.state.lock() {
            Ok(st) => {
                if st.last_seen_ms == 0 {
                    return 0;
                }
                now_ms().saturating_sub(st.last_seen_ms)
            }
            Err(_) => 0,
        }
    }

    /// `true` if the pipe is alive but silent past `lease_ms`.
    #[must_use]
    pub fn expired(&self, lease_ms: u64) -> bool {
        !self.eof() && self.idle_ms() > lease_ms
    }
}

/// Emergency opcode handler `fn(opcode, payload)` — runs in the
/// SUPERVISOR, fast + idempotent + lock-free (same as any rollback).
pub type TokenRollback = fn(u32, [u8; 28]);

/// Turnkey process supervisor managing satellite lifecycle and dead-token emergency rescue.
///
/// Phase 6 orphan contract lives below ([`OrphanGuard`]).
pub struct Supervisor;

impl Supervisor {
    /// Spawn `cmd` with `digest` written to the replacement process's stdin at startup.
    ///
    /// Caller builds the `Command` (exe + args + env — the crate never
    /// guesses); we pipe stdin, `write_all` the 64 B (fits any 4 KB
    /// kernel pipe buffer — never blocks), `drop(stdin)` for EOF, and
    /// hand back the live `Child`. The replacement process reads it on its first line
    /// via [`take_stdin_digest`].
    pub fn spawn_with_digest(
        mut cmd: std::process::Command,
        digest: &StateDigest,
    ) -> std::io::Result<std::process::Child> {
        use std::io::Write;
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&digest.bytes);
            // EOF: child's read_exact completes without deadlock.
        }
        Ok(child)
    }

    /// Inspect command line args and run the satellite supervisor loop if `--supervisor` is present.
    ///
    /// Flow:
    /// - If `args` contains `worker_flag` (e.g. `--worker`): returns `false` (this is the satellite worker).
    /// - If `args` does NOT contain `--supervisor`: returns `false` (normal un-supervised execution).
    /// - If `args` contains `--supervisor` and not `worker_flag`:
    ///   Spawns child with `worker_flag`, drains anonymous pipe, waits for death,
    ///   executes `on_dead_token` if worker died leaving an armed token, and returns `true` (exit main).
    pub fn run_satellite_guard<F>(worker_flag: &'static str, on_dead_token: F) -> bool
    where
        F: Fn(ProcessToken),
    {
        let args: Vec<String> = std::env::args().collect();
        let is_worker = args.iter().any(|a| a == worker_flag);
        let wants_supervisor = args.iter().any(|a| a == "--supervisor");

        if is_worker || !wants_supervisor {
            return false;
        }

        let mut clock = GenerationClock::new();
        let current_gen = clock.generation();
        println!("[SUPERVISOR:START] gen={current_gen} worker_flag={worker_flag}");

        let current_exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                eprintln!("[SUPERVISOR:ERROR] action=get_current_exe error=\"{e}\"");
                return true;
            }
        };

        // Forward args except --supervisor
        let mut child_cmd = std::process::Command::new(current_exe);
        child_cmd.arg(worker_flag);
        for arg in &args[1..] {
            if arg != "--supervisor" && arg != worker_flag {
                child_cmd.arg(arg);
            }
        }
        child_cmd.env("SNUBBER_SUPERVISED", "1");
        child_cmd.env("SNUBBER_GEN", current_gen.to_string());
        child_cmd.stdout(std::process::Stdio::piped());

        let mut child = match child_cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[SUPERVISOR:ERROR] action=spawn_child error=\"{e}\"");
                return true;
            }
        };

        let pid = child.id();
        println!("[SUPERVISOR:SPAWN] child_pid={pid} gen={current_gen}");

        if let Some(stdout) = child.stdout.take() {
            let drain = PipeDrain::spawn(stdout);
            let status = child.wait();
            println!("[SUPERVISOR:TERMINATED] child_pid={pid} status={status:?}");

            for _ in 0..50 {
                if drain.eof() {
                    break;
                }
                std::thread::yield_now();
            }

            if let Some(token) = drain.take_armed() {
                println!(
                    "[SUPERVISOR:RECOVER] child_pid={pid} rescue_token=0x{:X} opcode={} task={}",
                    token.task, token.opcode, token.task
                );
                on_dead_token(token);
            } else {
                println!("[SUPERVISOR:CLEAN] child_pid={pid} status=all_settled");
            }
        } else {
            let _ = child.wait();
        }

        let next_gen = clock.restart();
        println!("[SUPERVISOR:RESTART] lineage_bump_gen={next_gen}");
        true
    }
}

/// Orphan contract (Phase 6): parent death = satellites reaped.
///
/// The crate NAMES the guarantee; the APP implements it. Win32 needs
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`unsafe` + `windows-sys`) —
/// forbidden here by `forbid(unsafe_code)` + zero-dep. Unix needs
/// `prctl(PR_SET_PDEATHSIG)` (`libc`) — same veto. So the crate ships
/// the trait + a portable no-op; the real impl is an application-level pattern.
///
/// Owner (R1): the APP's supervisor setup. The crate never calls it —
/// it only types the promise so every satellite binary shares one.
pub trait OrphanGuard: Send + Sync {
    /// Arm: from now on, if THIS process dies, the OS reaps `pid`.
    /// Returns `false` when the platform cannot promise (caller logs).
    fn arm(&self, pid: u32) -> bool;
    /// Human name for logs (`"win32-job-object"`, `"prctl-pdeathsig"`).
    fn backend_name(&self) -> &'static str;
}

/// Portable no-op guard: promises nothing, says so honestly.
/// Parent death orphans satellites — same as raw `Command::spawn`.
/// Use when the deployment owns reaping (systemd, k8s, outer daemon).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NoOrphanGuard;

impl OrphanGuard for NoOrphanGuard {
    #[inline]
    fn arm(&self, _pid: u32) -> bool {
        false
    }
    #[inline]
    fn backend_name(&self) -> &'static str {
        "none (portable no-op: orphans possible)"
    }
}

/// Read the state snapshot from stdin at startup (replacement process side).
///
/// Deterministic `read_exact`: 64 B delivered -> `Some(StateDigest)`, fewer bytes
/// (torn write) or no stdin available -> `None`.
/// Call ONCE on the first line of `main` — subsequent calls observe EOF.
#[must_use]
pub fn take_stdin_digest() -> Option<StateDigest> {
    use std::io::Read;
    let mut buf = [0u8; STATE_LEN];
    if std::io::stdin().read_exact(&mut buf).is_ok() {
        Some(StateDigest::new(buf))
    } else {
        None
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// Full-vector LE canon: `to_le_bytes` is `const`, but `==` on arrays
    /// is not const-stable on MSRV 1.70 — so the byte-exact vector proof
    /// lives here at runtime (the scalar first byte is pinned in the
    /// `const` gate above; both must hold).
    #[test]
    fn wire_is_le_canon() {
        assert_eq!(0x12345678u32.to_le_bytes(), [0x78, 0x56, 0x34, 0x12]);
        assert_eq!(
            0x0102030405060708u64.to_le_bytes(),
            [8, 7, 6, 5, 4, 3, 2, 1]
        );
        // Round-trip: the drain decodes exactly what the satellite encoded.
        let token = ProcessToken::new(0x0102030405060708, 0x12345678, [0xAB; 28]);
        let wire = token.to_bytes();
        assert_eq!(&wire[0..4], b"SNUB");
        let body: [u8; 40] = wire[4..44].try_into().expect("40 B body");
        let back = ProcessToken::from_body_bytes(&body);
        assert_eq!(back, token, "LE round-trip must be bit-exact");
    }
}
