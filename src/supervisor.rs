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
use crate::memory::{MemoryVerdict, MemoryWatch};
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
#[non_exhaustive]
pub enum DeathCause {
    /// Pipe broke / handle signaled (`abort()`, panic-abort, SIGKILL).
    AbortUnwind,
    /// No traffic past the lease.
    HangTimeout,
    /// Reaped from outside (operator, OOM-killer, parent).
    KilledOutside,
    /// External memory sample crossed the hard limit (Phase 3):
    /// terminated WITHOUT draining — the process was already over budget.
    MemoryLimit,
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
            Self::MemoryLimit => "process exceeded hard memory limit",
        }
    }
}

/// Supervisor-side memory policy (Phase 3).
///
/// Pure `Copy` state over [`MemoryWatch`]: the
/// supervisor samples the child (its own platform call — `/proc`,
/// `GetProcessMemoryInfo`, cgroup) and asks this policy whether to kill.
/// `Soft` never kills: the worker refuses admissions through
/// [`EnterError::MemoryPressure`](crate::EnterError::MemoryPressure),
/// drains in-flight guards, hands off a [`StateDigest`] and exits for a
/// successor generation. Only `Hard` terminates without draining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoryPolicy {
    watch: MemoryWatch,
}

impl MemoryPolicy {
    /// `soft_kb` = drain threshold, `hard_kb` = kill threshold
    /// (`0` disables either; `hard < soft` clamps up to `soft`).
    #[must_use]
    pub const fn new(soft_kb: u64, hard_kb: u64) -> Self {
        Self {
            watch: MemoryWatch::new(soft_kb, hard_kb),
        }
    }

    /// Underlying watch (last sample, peak, limits).
    #[must_use]
    pub const fn watch(&self) -> MemoryWatch {
        self.watch
    }

    /// Record one child sample (KiB) and return its verdict.
    pub fn note(&mut self, used_kb: u64) -> MemoryVerdict {
        self.watch.note(used_kb)
    }

    /// Verdict for the last sample.
    #[must_use]
    pub const fn verdict(&self) -> MemoryVerdict {
        self.watch.verdict()
    }

    /// `true` only at the hard limit. Soft is a drain request, not a kill.
    #[must_use]
    pub const fn should_kill(&self) -> bool {
        self.watch.verdict().is_hard()
    }

    /// `Some(MemoryLimit)` when [`should_kill`](Self::should_kill).
    #[must_use]
    pub const fn death_cause(&self) -> Option<DeathCause> {
        if self.watch.verdict().is_hard() {
            Some(DeathCause::MemoryLimit)
        } else {
            None
        }
    }
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self::new(0, 0)
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
#[non_exhaustive]
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
    ///
    /// Generation semantics NOTE: the internal clock bumps only on
    /// AUTHORIZED restarts (`Restart { .. }`). [`crate::pool::PoolLane`]
    /// bumps its own clock on EVERY reap — under `RestartPolicy::Never` /
    /// a `GiveUp`, `RestartTracker::generation()` and
    /// `PoolLane::generation()` legitimately diverge. Each is consistent
    /// with its own meaning (restarts served vs lineages seen); do not
    /// compare them across the two types.
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
            while self.deaths_ms.len() > (self.window.max_restarts as usize).saturating_add(1) {
                self.deaths_ms.pop_front();
            }
            if self.deaths_ms.len() > self.window.max_restarts as usize {
                self.giveups = self.giveups.saturating_add(1);
                return RestartDecision::GiveUp {
                    deaths_in_window: u32::try_from(self.deaths_ms.len()).unwrap_or(u32::MAX),
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

// Token wire types + the frame scanner live in the core-only `drain`
// module (reachable from no_std); re-exported here so every historical
// `supervisor::` path keeps resolving.
use crate::drain::SnubScanner as IngestState;
pub use crate::drain::{
    ProcessToken, OP_COMMIT, OP_HEARTBEAT, OP_SNAPSHOT, OP_STATE, TOKEN_BODY_LEN, TOKEN_MAGIC,
    TOKEN_PAYLOAD_LEN,
};

impl ProcessToken {
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
    ///
    /// Stdout + env are only the DEFAULT transport: apps that own a socket,
    /// UART or pipe call [`ProcessToken::write_to`] directly.
    pub fn emit_if_supervised(&self) {
        if Self::is_supervised() {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = self.write_to(&mut lock);
            let _ = lock.flush();
        }
    }

    /// Write the 44 wire bytes to any sink — pipe, socket, UART bridge or
    /// in-memory buffer. No env lookup, no stdout coupling; errors are
    /// returned instead of swallowed.
    ///
    /// # Errors
    /// Whatever `out` returns.
    pub fn write_to<W: std::io::Write>(self, out: &mut W) -> std::io::Result<()> {
        out.write_all(&self.to_bytes())
    }
}

/// State digest payload: exactly 64 bytes (one CPU cache line).
pub const STATE_LEN: usize = 64;

// COMPILE-TIME GATES (hot-handoff wire math; the token gates live in the
// core `drain` module; these cover the std-side digest layout):
// * state snapshot = exactly one L1 cache line (64 B);
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

    /// Write the three `OP_STATE` frames to any sink (no env, no stdout).
    ///
    /// # Errors
    /// Whatever `out` returns.
    pub fn write_to<W: std::io::Write>(self, out: &mut W) -> std::io::Result<()> {
        for frame in self.to_frames() {
            frame.write_to(out)?;
        }
        Ok(())
    }

    /// Publish to stdout under a supervisor (child process side).
    /// No-op when unsupervised. Flushes — drain reads in real time.
    pub fn emit_if_supervised(&self) {
        if ProcessToken::is_supervised() {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = self.write_to(&mut lock);
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
/// `last_seen_ms`: boot-relative timestamp of the last valid 40 B frame
/// (token, commit, heartbeat OR state — any traffic proves the worker
/// pumps). `seen` is separate because timestamp `0` is a valid uptime.
#[derive(Debug, Default)]
struct DrainState {
    armed: Option<ProcessToken>,
    last_seen_ms: u64,
    seen: bool,
    /// Parsed-frame counters: total, arm-class, commit. Let a supervisor
    /// classify a dead worker (`commits > 0` = settled at least once).
    frames: u64,
    arms: u64,
    commits: u64,
    /// Last verified complete StateDigest.
    state: Option<StateDigest>,
    /// Reassembly scratch: chunks per seq (24/24/16) + got-mask + full CRC.
    scratch: [[u8; TOKEN_PAYLOAD_LEN]; 3],
    scratch_got: u8,
    scratch_full_crc: u32,
}

/// Monotonic millisecond clock supplied to a drain.
///
/// The default is [`now_ms`]; tests pass a counter and firmware passes its
/// `SysTick` read, so lease sweeps are driven by the same clock the app
/// already owns instead of a hidden `Instant`.
pub type ClockFn = fn() -> u64;

/// Monotonic millisecond clock (supervisor side, `std` backend).
///
/// Delegates to the node-side boot-relative `Instant` source so both
/// halves of Layer 4 share one origin. Never `SystemTime`. This is the
/// default [`ClockFn`]; callers that own a cheaper tick source can inject
/// their own via [`PipeDrain::manual_with_clock`].
#[inline]
pub fn now_ms() -> u64 {
    super::node::now_ms()
}

/// Frame counters observed by a drain since it started.
///
/// Lets a supervisor classify a dead worker without keeping per-frame
/// history: `commits > 0` means the worker settled at least once;
/// `arms > commits` means it was armed more often than it settled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainCounters {
    /// Total parsed 44-byte wire frames (40 B token body + 4 B `SNUB`
    /// magic) — arms, commits, heartbeats, state chunks.
    pub frames: u64,
    /// Arm-class frames (any opcode outside commit/heartbeat/state/
    /// snapshot — `OP_SNAPSHOT` is diagnostics and never counted here).
    pub arms: u64,
    /// `OP_COMMIT` frames.
    pub commits: u64,
}

#[derive(Debug)]
struct DrainInner {
    state: Mutex<DrainState>,
    ingest: Mutex<IngestState>,
}

/// Pipe drain: background reader keeping the kernel buffer empty.
///
/// Robust framing: scans for `TOKEN_MAGIC` ("SNUB"). Any non-token output
/// (e.g. `println!` logs) in stdout is transparently skipped without losing
/// frame alignment or crashing.
///
/// Known limitation (accepted): frames carry NO CRC (fixed 44 B wire,
/// zero-dep budget), so the literal magic `SNUB` embedded in a worker's
/// TEXT output within 40 following bytes can fake a frame; an unknown
/// opcode in that fake body arms a garbage emergency token (spurious
/// rollback) and the real frame is swallowed. Worker stdout must not
/// embed the magic followed by 40 binary-looking bytes — logs do not.
/// A per-frame CRC would close this and is a wire-format break, not a
/// patch.
///
/// Three drivers share one parser:
/// * [`PipeDrain::spawn`] — OS reader thread (default `std` flow);
/// * [`PipeDrain::drain_sync`] — blocking read to EOF (buffers, fuzzing);
/// * [`PipeDrain::feed`] — caller-pushed chunks (embedded UART, async
///   runtimes, tests). Call [`PipeDrain::finish`] when the stream ends.
///
/// Cloning is a cheap handle: every clone observes the same drain.
#[derive(Debug, Clone)]
pub struct PipeDrain {
    inner: Arc<DrainInner>,
    done: Arc<AtomicU64>,
    feeds_after_eof: Arc<AtomicU64>,
    clock: ClockFn,
}

impl PipeDrain {
    /// Empty drain driven only by [`PipeDrain::feed`] (no reader thread).
    #[must_use]
    pub fn manual() -> Self {
        Self::manual_with_clock(now_ms)
    }

    /// Empty `feed`-driven drain with an injected clock.
    ///
    /// The clock stamps `last_seen` and answers [`PipeDrain::idle_ms`], so
    /// lease sweeps are deterministic in tests and follow the firmware tick
    /// on embedded targets instead of a hidden `Instant`.
    ///
    /// ```
    /// use snubber::supervisor::PipeDrain;
    ///
    /// fn app_tick_ms() -> u64 {
    ///     0 // the firmware's SysTick counter, in milliseconds
    /// }
    ///
    /// let drain = PipeDrain::manual_with_clock(app_tick_ms);
    /// let frames = drain.feed(&[0u8; 7]); // any chunking, any size
    /// assert_eq!(frames, 0); // a torn frame is retained, not delivered
    /// drain.finish(); // EOF: rescue classification runs here
    /// assert_eq!(drain.feed(&[0u8; 1]), 0); // a closed drain accepts nothing
    /// assert_eq!(drain.feeds_after_eof(), 1);
    /// ```
    #[must_use]
    pub fn manual_with_clock(clock: ClockFn) -> Self {
        Self {
            inner: Arc::new(DrainInner {
                state: Mutex::new(DrainState::default()),
                ingest: Mutex::new(IngestState::default()),
            }),
            done: Arc::new(AtomicU64::new(0)),
            feeds_after_eof: Arc::new(AtomicU64::new(0)),
            clock,
        }
    }

    /// Spawn the drain on `reader`. Returns immediately, thread owns it.
    #[must_use]
    pub fn spawn<R: Read + Send + 'static>(reader: R) -> Self {
        Self::spawn_with_clock(reader, now_ms)
    }

    /// [`PipeDrain::spawn`] with an injected clock.
    #[must_use]
    pub fn spawn_with_clock<R: Read + Send + 'static>(reader: R, clock: ClockFn) -> Self {
        let drain = Self::manual_with_clock(clock);
        let worker = drain.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::with_capacity(4096, reader);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        worker.feed(&buf[..n]);
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            worker.finish();
        });
        drain
    }

    /// Synchronously process a reader to EOF without spawning a background OS thread.
    /// Ideal for in-memory byte buffers, fuzzing sweeps, and synchronous parsers.
    #[must_use]
    pub fn drain_sync<R: Read>(reader: R) -> Self {
        Self::drain_sync_with_clock(reader, now_ms)
    }

    /// [`PipeDrain::drain_sync`] with an injected clock.
    #[must_use]
    pub fn drain_sync_with_clock<R: Read>(mut reader: R, clock: ClockFn) -> Self {
        let drain = Self::manual_with_clock(clock);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    drain.feed(&buf[..n]);
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        drain.finish();
        drain
    }

    /// Push bytes through the incremental `SNUB` scanner.
    ///
    /// Accepts any chunking: one byte at a time, several frames per chunk,
    /// garbage prefixes, torn tails. Returns the number of complete frames
    /// delivered by THIS call; a partial frame is retained for the next one.
    /// A finished drain accepts nothing, returns `0`, and counts the
    /// rejected call — see [`PipeDrain::feeds_after_eof`].
    pub fn feed(&self, bytes: &[u8]) -> usize {
        // Lock-free fast path: a finished drain returns without touching
        // the mutex or calling the user clock. Advisory only — the
        // authoritative check below runs under `ingest`.
        if self.eof() {
            self.feeds_after_eof.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        // One clock read per chunk, before ANY drain lock. The ClockFn is
        // caller code; it must never run under a drain mutex.
        let now = (self.clock)();
        // Take the ingest lock BEFORE the second EOF check: `finish` also
        // takes it, so a concurrent close either fully precedes this chunk
        // or fully follows it — a finished drain truly accepts nothing.
        let mut ingest = match self.inner.ingest.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.eof() {
            self.feeds_after_eof.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        ingest.push(bytes, |token| self.apply_token_at(token, now))
    }

    /// Chunks refused because the drain had already reached `EOF`.
    ///
    /// A non-zero value after `finish` means a producer kept writing into a
    /// closed pipe: the bytes were dropped, not buffered.
    #[must_use]
    pub fn feeds_after_eof(&self) -> u64 {
        self.feeds_after_eof.load(Ordering::Relaxed)
    }

    /// Mark the stream finished (`EOF`). Idempotent, thread-safe; a torn
    /// partial frame is discarded, exactly like a reader that hit `EOF`.
    pub fn finish(&self) {
        // Serialize with `feed` through the ingest lock (see `feed`).
        let _ingest = match self.inner.ingest.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.done.store(1, Ordering::SeqCst);
    }

    /// Apply one parsed token under one short state lock.
    fn apply_token_at(&self, token: ProcessToken, now: u64) {
        let mut state = match self.inner.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply_token(&mut state, token, now);
    }

    /// Current reading of this drain's clock, ms.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    /// Take the last un-settled token, if any (call AFTER `EOF`/reap).
    ///
    /// Poison-safe: `DrainState` is plain counters + `Option` slots (no
    /// cross-field invariants a panic could tear), so a poisoned lock is
    /// recovered through the guard instead of silently dropping the
    /// rescue token.
    #[must_use]
    pub fn take_armed(&self) -> Option<ProcessToken> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .armed
            .take()
    }

    /// Parsed-frame counters (cheap snapshot under one short lock).
    #[must_use]
    pub fn counters(&self) -> DrainCounters {
        // Recover a poisoned lock: the counters are plain integers, so the
        // recovered value is exactly as trustworthy as before the panic.
        let state = match self.inner.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        DrainCounters {
            frames: state.frames,
            arms: state.arms,
            commits: state.commits,
        }
    }

    /// Take the last verified complete `StateDigest`, if any (Phase 3),
    /// consuming it — the next call returns `None`. Call after process
    /// exit and hand the bytes to the replacement process at startup
    /// (child stdin/env/file — the crate delivers, the app injects).
    /// Copy-out under one short lock — no alloc, never panics. Poison
    /// is recovered (`DrainState` holds no tearable invariants — see
    /// [`PipeDrain::take_armed`]).
    #[must_use]
    pub fn take_state(&self) -> Option<StateDigest> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .state
            .take()
    }

    /// Peek without taking (supervisor polls before spawning replacement process).
    #[must_use]
    pub fn last_state(&self) -> Option<StateDigest> {
        let state = match self.inner.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.state
    }

    /// `true` once the reader hit `EOF`/error or [`PipeDrain::finish`] ran.
    #[must_use]
    pub fn eof(&self) -> bool {
        self.done.load(Ordering::SeqCst) != 0
    }

    /// Milliseconds since the last valid frame (`0` if no frame was ever
    /// parsed — a silent pipe before its first frame is never expired).
    /// The supervisor turns `elapsed > lease_ms` (pipe alive, no traffic)
    /// into `DeathCause::HangTimeout`: `take_armed()` → `child.kill()`
    /// (`TerminateProcess`/SIGKILL from OUTSIDE) → `clock.restart()`.
    /// A reaped-then-hung PID is abandoned — the generation already moved.
    #[must_use]
    pub fn idle_ms(&self) -> u64 {
        // Read the clock BEFORE the lock: ClockFn is caller code and must
        // not run while the drain state mutex is held. A poisoned state
        // still holds a valid timestamp — recover, do not report 0.
        let now = (self.clock)();
        let state = match self.inner.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !state.seen {
            return 0;
        }
        now.saturating_sub(state.last_seen_ms)
    }

    /// `true` if the pipe is alive but silent past `lease_ms`.
    #[must_use]
    pub fn expired(&self, lease_ms: u64) -> bool {
        !self.eof() && self.idle_ms() > lease_ms
    }
}

/// Apply one parsed token to drain state under the caller's lock.
fn apply_token(state: &mut DrainState, token: ProcessToken, now_ms: u64) {
    state.last_seen_ms = now_ms;
    state.seen = true;
    state.frames = state.frames.saturating_add(1);
    if token.opcode == OP_COMMIT || token.opcode == OP_HEARTBEAT {
        if token.opcode == OP_COMMIT {
            state.commits = state.commits.saturating_add(1);
            state.armed = None;
        }
    } else if token.opcode == OP_STATE {
        // Hot handoff chunk: seq = task (0/1/2).
        // Layout: payload[0..take] = data (24/24/16).
        // Frame 2 carries the authoritative full-digest CRC at payload[16..20].
        // Frame CRC at payload[24..28].
        // A sequence id outside the three-chunk layout is malformed, not
        // "chunk 2": clamping it would let an unrelated frame overwrite the
        // tail chunk and its CRC fields.
        let seq = match usize::try_from(token.task) {
            Ok(seq) if seq < state.scratch.len() => seq,
            _ => return,
        };
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
            state.scratch[seq][0..take].copy_from_slice(&token.payload[0..take]);
            if seq == 2 {
                let mut full = [0u8; 4];
                full.copy_from_slice(&token.payload[16..20]);
                state.scratch_full_crc = u32::from_le_bytes(full);
            }
            state.scratch_got |= 1 << seq;
            if state.scratch_got == 0b111 {
                let mut bytes = [0u8; STATE_LEN];
                bytes[0..24].copy_from_slice(&state.scratch[0][0..24]);
                bytes[24..48].copy_from_slice(&state.scratch[1][0..24]);
                bytes[48..64].copy_from_slice(&state.scratch[2][0..16]);
                let candidate = StateDigest::new(bytes);
                if candidate.crc() == state.scratch_full_crc {
                    state.state = Some(candidate);
                }
                // Reset the mask so subsequent digests reassemble cleanly.
                state.scratch_got = 0;
            }
        }
        // Corrupt or mismatched frame: dropped silently, the replacement
        // process gets the last verified whole state.
    } else if token.opcode == OP_SNAPSHOT {
        // Cold diagnostics marker: arms nothing, clears nothing (see
        // OP_SNAPSHOT). A snapshot must never be mistaken for an armed
        // transaction; the next OP_COMMIT or emergency opcode does that.
    } else {
        state.arms = state.arms.saturating_add(1);
        state.armed = Some(token);
    }
}

/// Emergency opcode handler `fn(opcode, payload)` — runs in the
/// SUPERVISOR, fast + idempotent + lock-free (same as any rollback).
pub type TokenRollback = fn(u32, [u8; TOKEN_PAYLOAD_LEN]);

/// External RSS sampler: child pid -> resident KiB (the APP's platform call).
type MemorySampler<'a> = &'a dyn Fn(u32) -> Option<u64>;

/// Turnkey process supervisor managing child process lifecycle and dead-token emergency rescue.
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

    /// Inspect command line args and run the child process supervisor loop if `--supervisor` is present.
    ///
    /// Flow:
    /// - If `args` contains `worker_flag` (e.g. `--worker`): returns `false` (this is the child worker process).
    /// - If `args` does NOT contain `--supervisor`: returns `false` (normal un-supervised execution).
    /// - If `args` contains `--supervisor` and not `worker_flag`:
    ///   Spawns child with `worker_flag`, drains anonymous pipe, waits for death,
    ///   executes `on_dead_token` if worker died leaving an armed token, and returns `true` (exit main).
    ///
    /// Delegates to [`Self::run_supervised_guard_with_lease`] with `lease_ms = 0` (indefinite wait).
    pub fn run_supervised_guard<F>(worker_flag: &'static str, on_dead_token: F) -> bool
    where
        F: Fn(ProcessToken),
    {
        Self::run_supervised_guard_with_lease(worker_flag, 0, on_dead_token)
    }

    /// Supervised guard with a configurable terminal EOF grace.
    ///
    /// The default [`Self::run_supervised_guard`] waits at most
    /// [`crate::pool::DEFAULT_EOF_GRACE_MS`] after process death for the
    /// pipe to flush its last frames before classifying the death. A
    /// slow-flushing child (big buffered stdout) can be misreported CLEAN
    /// when its rescue token is still in flight — pass a larger
    /// `eof_grace_ms` here. Mirrors the pool's `with_eof_grace`.
    pub fn run_supervised_guard_with_grace<F>(
        worker_flag: &'static str,
        lease_ms: u64,
        eof_grace_ms: u64,
        on_dead_token: F,
    ) -> bool
    where
        F: Fn(ProcessToken),
    {
        let argv: Vec<String> = std::env::args().collect();
        Self::run_loop(
            worker_flag,
            &argv,
            lease_ms,
            None,
            eof_grace_ms,
            on_dead_token,
        )
    }

    /// Explicit-argv form of [`Self::run_supervised_guard`].
    ///
    /// `argv` is the FULL argument vector, index 0 is the program name
    /// (identical shape to `std::env::args()`). Use it from tests,
    /// embedded launchers, or shells that already parsed their own
    /// command line instead of inheriting the process argv.
    pub fn run_supervised_guard_argv<F>(
        worker_flag: &'static str,
        argv: &[String],
        on_dead_token: F,
    ) -> bool
    where
        F: Fn(ProcessToken),
    {
        Self::run_supervised_guard_with_lease_argv(worker_flag, argv, 0, on_dead_token)
    }

    /// Inspect command line args and run the child process supervisor loop with an active silence lease (`lease_ms`).
    ///
    /// If `lease_ms > 0` and the child worker is silent (pipe open but no tokens/heartbeats)
    /// for longer than `lease_ms`, the supervisor terminates the child process externally
    /// via OS process termination (`child.kill()`), reclaims any armed `ProcessToken`
    /// via [`PipeDrain::take_armed`], executes `on_dead_token`, and increments the generation clock.
    pub fn run_supervised_guard_with_lease<F>(
        worker_flag: &'static str,
        lease_ms: u64,
        on_dead_token: F,
    ) -> bool
    where
        F: Fn(ProcessToken),
    {
        let argv: Vec<String> = std::env::args().collect();
        Self::run_loop(
            worker_flag,
            &argv,
            lease_ms,
            None,
            crate::pool::DEFAULT_EOF_GRACE_MS,
            on_dead_token,
        )
    }

    /// Explicit-argv form of [`Self::run_supervised_guard_with_lease`].
    ///
    /// `argv[0]` is the program name; `--supervisor` and `worker_flag`
    /// are stripped before forwarding to the child.
    pub fn run_supervised_guard_with_lease_argv<F>(
        worker_flag: &'static str,
        argv: &[String],
        lease_ms: u64,
        on_dead_token: F,
    ) -> bool
    where
        F: Fn(ProcessToken),
    {
        Self::run_loop(
            worker_flag,
            argv,
            lease_ms,
            None,
            crate::pool::DEFAULT_EOF_GRACE_MS,
            on_dead_token,
        )
    }

    /// Supervised loop with an external memory sampler (Phase 3).
    ///
    /// `sample(pid)` runs in the supervisor loop (every ~10 ms alongside the
    /// lease check) and returns the child's resident KiB — the platform call
    /// is the APP's (`/proc/<pid>/status`, `GetProcessMemoryInfo`, cgroup);
    /// the crate stays `#![forbid(unsafe_code)]`.
    ///
    /// * `Soft` — logged once as `[SUPERVISOR:MEMORY_DRAIN]`: telemetry for
    ///   the operator, NOT a control message — the pipe is one-way
    ///   (child → out), the supervisor never signals the worker. The child
    ///   is expected to sample its own memory (`SnubberNode` soft limit →
    ///   `crate::EnterError::MemoryPressure`) and drain itself. Never killed.
    /// * `Hard` — `child.kill()` WITHOUT draining and
    ///   [`DeathCause::MemoryLimit`] stamped via [`death_event`].
    pub fn run_supervised_guard_with_memory<F, S>(
        worker_flag: &'static str,
        lease_ms: u64,
        policy: MemoryPolicy,
        sample: S,
        on_dead_token: F,
    ) -> bool
    where
        F: Fn(ProcessToken),
        S: Fn(u32) -> Option<u64>,
    {
        let argv: Vec<String> = std::env::args().collect();
        Self::run_loop(
            worker_flag,
            &argv,
            lease_ms,
            Some((policy, &sample)),
            crate::pool::DEFAULT_EOF_GRACE_MS,
            on_dead_token,
        )
    }

    /// Explicit-argv form of [`Self::run_supervised_guard_with_memory`].
    pub fn run_supervised_guard_with_memory_argv<F, S>(
        worker_flag: &'static str,
        argv: &[String],
        lease_ms: u64,
        policy: MemoryPolicy,
        sample: S,
        on_dead_token: F,
    ) -> bool
    where
        F: Fn(ProcessToken),
        S: Fn(u32) -> Option<u64>,
    {
        Self::run_loop(
            worker_flag,
            argv,
            lease_ms,
            Some((policy, &sample)),
            crate::pool::DEFAULT_EOF_GRACE_MS,
            on_dead_token,
        )
    }

    fn run_loop<F>(
        worker_flag: &'static str,
        args: &[String],
        lease_ms: u64,
        memory: Option<(MemoryPolicy, MemorySampler<'_>)>,
        eof_grace_ms: u64,
        on_dead_token: F,
    ) -> bool
    where
        F: Fn(ProcessToken),
    {
        let mut memory = memory;
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

        // Forward args except --supervisor and our own worker flag.
        // `skip(1)`: index 0 is the program name; an empty custom argv
        // simply forwards nothing.
        let mut child_cmd = std::process::Command::new(current_exe);
        child_cmd.arg(worker_flag);
        for arg in args.iter().skip(1) {
            if arg != "--supervisor" && arg != worker_flag {
                child_cmd.arg(arg);
            }
        }
        child_cmd.env("SNUBBER_SUPERVISED", "1");
        child_cmd.env("SNUBBER_GEN", current_gen.to_string());
        child_cmd.stdout(std::process::Stdio::piped());
        // The pipe is one-way (child -> out); the gen-1 child never
        // receives a stdin digest (that is `spawn_with_digest`'s contract
        // for successors). Null stdin so a worker that follows the
        // `take_stdin_digest` doc reads EOF immediately instead of
        // blocking on the supervisor's inherited TTY forever.
        child_cmd.stdin(std::process::Stdio::null());

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

            let mut cause: Option<DeathCause> = None;
            if lease_ms > 0 || memory.is_some() {
                // Active watchdog loop: exit, silence past `lease_ms`,
                // or an external memory sample crossing the hard limit.
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            println!("[SUPERVISOR:TERMINATED] child_pid={pid} status={status:?}");
                            break;
                        }
                        Ok(None) => {
                            if lease_ms > 0 && drain.expired(lease_ms) {
                                eprintln!(
                                    "[SUPERVISOR:HANG_TIMEOUT] child_pid={pid} idle_ms={} lease_ms={lease_ms} action=terminate_process",
                                    drain.idle_ms()
                                );
                                let _ = child.kill();
                                let status = child.wait();
                                println!("[SUPERVISOR:KILLED] child_pid={pid} status={status:?}");
                                cause = Some(DeathCause::HangTimeout);
                                break;
                            }
                            if let Some((policy, sample)) = memory.as_mut() {
                                if let Some(used_kb) = sample(pid) {
                                    let was_soft = policy.verdict() == MemoryVerdict::Soft;
                                    let verdict = policy.note(used_kb);
                                    let watch = policy.watch();
                                    if verdict.is_hard() {
                                        eprintln!(
                                            "[SUPERVISOR:MEMORY_LIMIT] child_pid={pid} used_kb={used_kb} peak_kb={} hard_kb={} action=terminate_process_no_drain",
                                            watch.peak_kb(),
                                            watch.hard_kb()
                                        );
                                        let _ = child.kill();
                                        let status = child.wait();
                                        println!(
                                            "[SUPERVISOR:KILLED] child_pid={pid} status={status:?}"
                                        );
                                        cause = policy.death_cause();
                                        break;
                                    }
                                    if verdict == MemoryVerdict::Soft && !was_soft {
                                        // Telemetry, not a control message: the
                                        // pipe is one-way (child -> out), the
                                        // supervisor cannot signal the worker.
                                        // The CHILD is expected to sample its own
                                        // memory (`SnubberNode` soft limit ->
                                        // `EnterError::MemoryPressure`) and drain
                                        // itself; this line tells the operator to
                                        // expect a graceful exit and that the
                                        // supervisor will NOT kill.
                                        eprintln!(
                                            "[SUPERVISOR:MEMORY_DRAIN] child_pid={pid} used_kb={used_kb} soft_kb={} action=no_kill_self_drain_expected",
                                            watch.soft_kb()
                                        );
                                    }
                                }
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => {
                            eprintln!("[SUPERVISOR:WAIT_ERROR] child_pid={pid} error=\"{e}\"");
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                    }
                }
            } else {
                let status = child.wait();
                println!("[SUPERVISOR:TERMINATED] child_pid={pid} status={status:?}");
            }

            if let Some(cause) = cause {
                println!(
                    "[SUPERVISOR:DEATH_CAUSE] child_pid={pid} cause=\"{}\"",
                    cause.reason()
                );
            }

            // Bounded EOF grace (default ceiling: the pool reaper's
            // `crate::pool::DEFAULT_EOF_GRACE_MS`; configurable via
            // `run_supervised_guard_with_grace`, same role as the pool's
            // `with_eof_grace`): a bare yield-spin never observes a pipe
            // that still holds buffered bytes — the rescue token would be
            // missed and the death reported CLEAN while the child's last
            // words are still in flight. Same boundary as the pool
            // (`elapsed > grace` = give up): spelled here as `<= grace` =
            // keep waiting, in pool/lane.rs as `> grace` = stop —
            // equivalent, kept so both grep alike.
            let eof_started = drain.now_ms();
            while !drain.eof() && drain.now_ms().saturating_sub(eof_started) <= eof_grace_ms {
                std::thread::sleep(std::time::Duration::from_millis(1));
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
        // One-shot supervision: this call does NOT spawn a successor —
        // it bumps the lineage so the NEXT invocation (the caller's
        // restart loop) mints a fresh generation, then returns.
        println!("[SUPERVISOR:LINEAGE_BUMP] gen={next_gen} restart=caller_decides");
        true
    }

    /// Backward-compatible alias for [`Supervisor::run_supervised_guard`].
    #[deprecated(since = "0.5.0", note = "use `run_supervised_guard` instead")]
    pub fn run_satellite_guard<F>(worker_flag: &'static str, on_dead_token: F) -> bool
    where
        F: Fn(ProcessToken),
    {
        Self::run_supervised_guard(worker_flag, on_dead_token)
    }
}

/// Orphan contract (Phase 6): parent death = child processes reaped.
///
/// The crate NAMES the guarantee; the APP implements it. Win32 needs
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`unsafe` + `windows-sys`) —
/// forbidden here by `forbid(unsafe_code)` + zero-dep. Unix needs
/// `prctl(PR_SET_PDEATHSIG)` (`libc`) — same veto. So the crate ships
/// the trait + a portable no-op; the real impl is an application-level pattern.
///
/// Owner (R1): the APP's supervisor setup. The crate never calls it —
/// it only types the promise so every child process binary shares one.
pub trait OrphanGuard: Send + Sync {
    /// Arm: from now on, if THIS process dies, the OS reaps `pid`.
    /// Returns `false` when the platform cannot promise (caller logs).
    fn arm(&self, pid: u32) -> bool;
    /// Human name for logs (`"win32-job-object"`, `"prctl-pdeathsig"`).
    fn backend_name(&self) -> &'static str;
}

/// Portable no-op guard: promises nothing, says so honestly.
/// Parent death orphans child processes — same as raw `Command::spawn`.
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// tests exercise the panic paths on purpose; the unwinds ARE the contract
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
        // Round-trip: the drain decodes exactly what the child process encoded.
        let token = ProcessToken::new(0x0102030405060708, 0x12345678, [0xAB; 28]);
        let wire = token.to_bytes();
        assert_eq!(&wire[0..4], b"SNUB");
        let body: [u8; 40] = wire[4..44].try_into().expect("40 B body");
        let back = ProcessToken::from_body_bytes(&body);
        assert_eq!(back, token, "LE round-trip must be bit-exact");
    }
}
