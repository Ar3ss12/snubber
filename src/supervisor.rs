//! Lifecycle supervisor: Path-1 spine + Path-2 token scalpel.
//!
//! PATH-1 (spine, 90%): death/hang = reap the process + emit
//! `SnubberEvent::Aborted { ProcessDied }` + restart at `generation + 1`.
//! [`EpochGate`](crate::EpochGate) cuts the zombie packets itself.
//!
//! PATH-2 (scalpel, 10%): last-armed 40-byte token over an anonymous
//! `std::process` pipe (ZERO `unsafe`, ZERO deps).
//!
//! PIPE-CHOKE CONTRACT: drain the pipe in a background reader thread in
//! real time (kernel buffer is 4-64 KB; 1600 x 40 B stall the WORKER on
//! `write()`). Drain keeps ONE last-armed token, clears on `OP_COMMIT`.
//! On `EOF` an un-cleared token fires the emergency rollback.
//!
//! Dumb as a rail: spawn / drain / wait / emit / restart. No policies.

use super::{SnubberEvent, TransactionHeader};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Monotonic generation clock for ONE supervised lineage.
#[derive(Debug)]
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

/// Build the `Aborted` death event (Path-1 emit).
/// v0.4 honest: death is `ProcessDied`, never masked (D3 closed).
#[inline]
#[must_use]
pub fn death_event(header: TransactionHeader, cause: DeathCause) -> SnubberEvent<()> {
    SnubberEvent::Aborted {
        header,
        reason: cause.reason(),
    }
}

/// Drain state: exactly ONE slot (last un-settled token) + liveness clock.
///
/// `last_seen_ms`: wall-clock of the last valid 40 B frame (token,
/// commit OR heartbeat — any traffic proves the worker pumps).
/// `0` = nothing seen yet.
#[derive(Debug, Default)]
struct DrainState {
    armed: Option<ProcessToken>,
    last_seen_ms: u64,
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
            let mut reader = reader;
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

    /// Take the last un-settled token, if any (call AFTER `EOF`/reap).
    #[must_use]
    pub fn take_armed(&self) -> Option<ProcessToken> {
        self.state.lock().ok().and_then(|mut st| st.armed.take())
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
pub struct Supervisor;

impl Supervisor {
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
        println!("============================================================");
        println!(">>> [SNUBBER SUPERVISOR] Started! Generation = {current_gen}");
        println!(">>> Spawning satellite worker ({worker_flag}) with anonymous pipe drain...");
        println!("============================================================");

        let current_exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                eprintln!(">>> [SNUBBER SUPERVISOR] Failed to get current_exe: {e}");
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
                eprintln!(">>> [SNUBBER SUPERVISOR] Failed to spawn satellite child: {e}");
                return true;
            }
        };

        let pid = child.id();
        println!(">>> [SNUBBER SUPERVISOR] Satellite running! PID: {pid}");

        if let Some(stdout) = child.stdout.take() {
            let drain = PipeDrain::spawn(stdout);
            let status = child.wait();
            println!("\n>>> [SNUBBER SUPERVISOR] Satellite reaped! Status: {status:?}");

            for _ in 0..50 {
                if drain.eof() {
                    break;
                }
                std::thread::yield_now();
            }

            if let Some(token) = drain.take_armed() {
                println!(
                    ">>> [SNUBBER SUPERVISOR] Found un-settled token: opcode={}, task={}",
                    token.opcode, token.task
                );
                on_dead_token(token);
            } else {
                println!(">>> [SNUBBER SUPERVISOR] All transactions settled cleanly.");
            }
        } else {
            let _ = child.wait();
        }

        let next_gen = clock.restart();
        println!(">>> [SNUBBER SUPERVISOR] Lineage bumped to gen={next_gen}. Done.");
        println!("============================================================");
        true
    }
}
