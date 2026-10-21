//! Cold diagnostics: snapshot codec, anomaly policy, health render, registry.
//!
//! Layering (sterility first):
//! * `core`-only: [`SnapshotV1`] (`#[repr(C)]`, fixed 512 B, LE codec +
//!   CRC32), [`AnomalyPolicy`], [`AnomalyKind`], [`Anomaly`],
//!   [`diagnostic_hash`] (FNV-1a), [`write_health`] (`fmt::Write`, no
//!   alloc), CRC helpers. No `std`, no `alloc`, no threads, no files.
//! * `std`-only: [`find_anomalies`] (pure over a `stats()` snapshot),
//!   [`NodeSet`] (explicit `&Node` list, never a `static`),
//!   [`print_health`], [`diagnostic_line`], [`assert_no_anomalies`].
//!
//! The crate gives bytes and math; the APP gives threads, paths and
//! network. `SnapshotV1::encode` + your own 10-second flusher is the whole "cold layer".

use core::fmt;

#[cfg(feature = "std")]
use super::{SnubberNode, SnubberStats};

/// Snapshot wire size: exactly 512 bytes, zero-padded, CRC32 in last 4.
pub const SNAPSHOT_LEN: usize = 512;
/// Snapshot format version (bytes 4..8, LE).
/// v3: `pulse_stalls` + `max_pulse_lag_ms` (Front 4: missed WorkerPulse
/// verdicts land in the cold frame). v2/v1 frames refuse as
/// `BadVersion`, never half-parse.
pub const SNAPSHOT_VERSION: u32 = 3;
/// Snapshot magic: `b"SNUB"` (bytes 0..4).
pub const SNAPSHOT_MAGIC: [u8; 4] = *b"SNUB";
/// CRC covers bytes `0..508`; stored LE at `508..512`.
pub const SNAPSHOT_CRC_RANGE: usize = 508;

// COMPILE-TIME GATES (cold-frame wire math, always compiled):
// the 512 B frame = 508 B payload + 4 B CRC; the `reported` tail must
// track `FaultKind::COUNT` (v2 grew 11 -> 12 for Preempted);
// v3 appends `pulse_stalls` + `max_pulse_lag_ms` (Front 4).
const _: () = {
    assert!(
        SNAPSHOT_LEN == 512,
        "CRITICAL: SNAPSHOT_LEN must be exactly 512 bytes"
    );
    assert!(
        SNAPSHOT_CRC_RANGE + 4 == SNAPSHOT_LEN,
        "CRITICAL: snapshot CRC must sit in the last 4 bytes of the frame"
    );
};

/// Fixed-shape cold snapshot of one node. `#[repr(C)]`, `Copy`, no
/// references — safe to `encode()` on a cold path and `write()` from the
/// app's own flusher thread. The node never touches the filesystem.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SnapshotV1 {
    /// Total guards armed (`total_started`).
    pub total_started: u64,
    /// Clean commits.
    pub committed: u64,
    /// Planned aborts.
    pub explicit_aborts: u64,
    /// Implicit drops (catastrophic: bug/panic path).
    pub implicit_drops: u64,
    /// Input validation rejections (PoisonedPayload).
    pub poisoned_rejected: u64,
    /// Stale-epoch cuts.
    pub stale_epochs_rejected: u64,
    /// Bulkhead rejections.
    pub bulkhead_rejections: u64,
    /// Evicted-oldest events.
    pub evicted: u64,
    /// Phase 1 preemptions: `Critical` over pure-`Background` (own column).
    /// Zero in v1 frames (v1 decodes refuse as `BadVersion`, never guess).
    pub preempted: u64,
    /// Breaker fast rejections while OPEN.
    pub breaker_rejections: u64,
    /// Breaker trips (Closed -> Open transitions).
    pub breaker_trips: u64,
    /// Watchdog timeouts.
    pub timeouts_fired: u64,
    /// Token-bucket rate rejections (OWN column, never merged).
    pub rate_rejected: u64,
    /// Sliding-window outcomes.
    pub window_outcomes: u64,
    /// Sliding-window failures.
    pub window_failures: u64,
    /// Mutex poison recoveries (goal: 0).
    pub poison_recoveries: u64,
    /// Supervised generation (app-filled; 0 = unsupervised).
    pub generation: u64,
    /// Per-kind explicit-report counters (`FaultKind::index` order).
    pub reported: [u64; super::FaultKind::COUNT],
    /// Missed `WorkerPulse` verdicts (Front 4: watcher saw a frozen tick
    /// past the lease). Zero in v2 frames (refuse as `BadVersion`).
    pub pulse_stalls: u64,
    /// Worst pulse lag observed, ms (saturating max).
    pub max_pulse_lag_ms: u64,
    /// Breaker position: 0 = Closed, 1 = Open, 2 = HalfOpen.
    pub breaker_state: u8,
}

impl SnapshotV1 {
    /// Catastrophic fault count: panic + timeout +
    /// process-died reports + implicit drops.
    #[must_use]
    #[inline]
    pub fn catastrophic_faults(&self) -> u64 {
        self.reported[super::FaultKind::Panic.index()]
            .saturating_add(self.reported[super::FaultKind::Timeout.index()])
            .saturating_add(self.reported[super::FaultKind::ProcessDied.index()])
            .saturating_add(self.implicit_drops)
    }

    /// Capacity shedding: bulkhead + evicted (worker capacity saturated).
    #[must_use]
    pub fn shed_bulkhead(&self) -> u64 {
        self.bulkhead_rejections.saturating_add(self.evicted)
    }

    /// Operational shedding: cooperative preemptions (own column, never merged).
    #[must_use]
    pub fn shed_preempted(&self) -> u64 {
        self.preempted
    }

    /// Operational shedding: token-bucket spam (own column forever).
    #[must_use]
    pub fn shed_rate(&self) -> u64 {
        self.rate_rejected
    }

    /// Operational shedding: stale / desync cuts.
    #[must_use]
    pub fn shed_stale(&self) -> u64 {
        self.stale_epochs_rejected
    }

    /// Total admission attempts observed.
    #[must_use]
    pub fn admission_total(&self) -> u64 {
        self.total_started
            .saturating_add(self.bulkhead_rejections)
            .saturating_add(self.stale_epochs_rejected)
            .saturating_add(self.breaker_rejections)
            .saturating_add(self.rate_rejected)
            .saturating_add(self.poisoned_rejected)
            .saturating_add(self.evicted)
    }

    /// Encode to exactly 512 bytes: magic + version + LE fields, zero
    /// padding, CRC32-IEEE over `0..508` stored at `508..512`.
    #[must_use]
    pub fn encode(self) -> [u8; SNAPSHOT_LEN] {
        let mut out = [0u8; SNAPSHOT_LEN];
        out[0..4].copy_from_slice(&SNAPSHOT_MAGIC);
        out[4..8].copy_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        let mut o = 8usize;
        macro_rules! w64 {
            ($v:expr) => {{
                out[o..o + 8].copy_from_slice(&$v.to_le_bytes());
                o += 8;
            }};
        }
        w64!(self.total_started);
        w64!(self.committed);
        w64!(self.explicit_aborts);
        w64!(self.implicit_drops);
        w64!(self.poisoned_rejected);
        w64!(self.stale_epochs_rejected);
        w64!(self.bulkhead_rejections);
        w64!(self.evicted);
        w64!(self.preempted);
        w64!(self.breaker_rejections);
        w64!(self.breaker_trips);
        w64!(self.timeouts_fired);
        w64!(self.rate_rejected);
        w64!(self.window_outcomes);
        w64!(self.window_failures);
        w64!(self.poison_recoveries);
        w64!(self.generation);
        for v in self.reported {
            w64!(v);
        }
        w64!(self.pulse_stalls);
        w64!(self.max_pulse_lag_ms);
        out[o] = self.breaker_state;
        let crc = crc32_ieee(&out[..SNAPSHOT_CRC_RANGE]);
        out[SNAPSHOT_CRC_RANGE..SNAPSHOT_LEN].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decode + verify: magic, version, CRC. Any mismatch is a hard
    /// refusal — never a half-parsed snapshot.
    pub fn decode(bytes: &[u8; SNAPSHOT_LEN]) -> Result<Self, SnapshotError> {
        if bytes[0..4] != SNAPSHOT_MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != SNAPSHOT_VERSION {
            return Err(SnapshotError::BadVersion(version));
        }
        let want = u32::from_le_bytes([
            bytes[SNAPSHOT_CRC_RANGE],
            bytes[SNAPSHOT_CRC_RANGE + 1],
            bytes[SNAPSHOT_CRC_RANGE + 2],
            bytes[SNAPSHOT_CRC_RANGE + 3],
        ]);
        let got = crc32_ieee(&bytes[..SNAPSHOT_CRC_RANGE]);
        if want != got {
            return Err(SnapshotError::BadCrc { want, got });
        }
        let mut o = 8usize;
        macro_rules! r64 {
            () => {{
                let v = u64::from_le_bytes([
                    bytes[o],
                    bytes[o + 1],
                    bytes[o + 2],
                    bytes[o + 3],
                    bytes[o + 4],
                    bytes[o + 5],
                    bytes[o + 6],
                    bytes[o + 7],
                ]);
                o += 8;
                v
            }};
        }
        let mut snap = Self {
            total_started: r64!(),
            committed: r64!(),
            explicit_aborts: r64!(),
            implicit_drops: r64!(),
            poisoned_rejected: r64!(),
            stale_epochs_rejected: r64!(),
            bulkhead_rejections: r64!(),
            evicted: r64!(),
            preempted: r64!(),
            breaker_rejections: r64!(),
            breaker_trips: r64!(),
            timeouts_fired: r64!(),
            rate_rejected: r64!(),
            window_outcomes: r64!(),
            window_failures: r64!(),
            poison_recoveries: r64!(),
            generation: r64!(),
            reported: [0; super::FaultKind::COUNT],
            pulse_stalls: 0,
            max_pulse_lag_ms: 0,
            breaker_state: 0,
        };
        for v in snap.reported.iter_mut() {
            *v = r64!();
        }
        snap.pulse_stalls = r64!();
        snap.max_pulse_lag_ms = r64!();
        snap.breaker_state = bytes[o];
        Ok(snap)
    }

    /// Build from a live stats snapshot (cold path: call on a cached
    /// `stats()`, never per-CPS). `generation`: supervisor lineage or 0.
    #[cfg(feature = "std")]
    #[must_use]
    pub fn from_stats(stats: &super::SnubberStats, generation: u64) -> Self {
        Self {
            total_started: stats.total_started,
            committed: stats.committed,
            explicit_aborts: stats.explicit_aborts,
            implicit_drops: stats.implicit_drops,
            poisoned_rejected: stats.poisoned_rejected,
            stale_epochs_rejected: stats.stale_epochs_rejected,
            bulkhead_rejections: stats.bulkhead_rejections,
            evicted: stats.evicted,
            preempted: stats.preempted,
            breaker_rejections: stats.breaker_rejections,
            breaker_trips: stats.breaker_trips,
            timeouts_fired: stats.timeouts_fired,
            rate_rejected: stats.rate_rejected,
            window_outcomes: stats.window_outcomes,
            window_failures: stats.window_failures,
            poison_recoveries: stats.poison_recoveries,
            generation,
            reported: stats.reported,
            pulse_stalls: stats.pulse_stalls,
            max_pulse_lag_ms: stats.max_pulse_lag_ms,
            breaker_state: match stats.breaker_state {
                super::BreakerState::Closed => 0,
                super::BreakerState::Open => 1,
                super::BreakerState::HalfOpen => 2,
            },
        }
    }
}

/// Snapshot decode refusal (hard, never half-parsed).
#[must_use = "a SnapshotError names why the cold frame refused (BadMagic/BadVersion/BadCrc) — match it, never drop it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapshotError {
    /// First 4 bytes are not `b"SNUB"`.
    BadMagic,
    /// Version mismatch (carries the found version).
    BadVersion(u32),
    /// CRC32 mismatch.
    BadCrc {
        /// CRC stored in the frame.
        want: u32,
        /// CRC computed over `0..508`.
        got: u32,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::BadMagic => write!(f, "snapshot: bad magic (not SNUB)"),
            Self::BadVersion(v) => write!(f, "snapshot: bad version {v}"),
            Self::BadCrc { want, got } => {
                write!(f, "snapshot: bad crc want={want:08x} got={got:08x}")
            }
        }
    }
}

/// CRC32-IEEE (poly `0xEDB88320`), bitwise, no table, `core`-only.
#[must_use]
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// FNV-1a 32-bit over node name + snapshot counters: the one-line
/// diagnosis hash for bug reports (`[DIAG 0x8FA3 ...]`). `core`-only.
#[must_use]
pub fn diagnostic_hash(node: &str, snap: &SnapshotV1) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    let mut mix = |b: u8| {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    };
    for &b in node.as_bytes() {
        mix(b);
    }
    for &w in &[
        snap.catastrophic_faults(),
        snap.shed_bulkhead(),
        snap.shed_rate(),
        snap.shed_stale(),
        snap.breaker_trips,
        snap.generation,
    ] {
        for &b in &w.to_le_bytes() {
            mix(b);
        }
    }
    h
}

/// Anomaly verdict kind (cold math, no IO).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnomalyKind {
    /// `shed_bulkhead / total > bottleneck_pct`: slow worker, raise the
    /// limit or shorten the critical section.
    Bottleneck,
    /// `shed_rate / total > flood_pct`: external spam / key chatter.
    Flood,
    /// `shed_stale > 0`: stale packet from superseded generation.
    Desync,
    /// `catastrophic_faults > 0`: catastrophic failure — algorithmic defect.
    Catastrophic,
    /// `shed_preempted / total > flood_pct`: seniors constantly evict
    /// juniors — either the Background lane is starved by design or
    /// priorities are inverted upstream. Operational shedding, never catastrophic.
    PreemptStorm,
    /// `pulse_stalls > 0`: the worker's heartbeat froze past the lease
    /// (Front 4). The engine lagged but never died — `count` = missed
    /// verdicts, `pct` carries `max_pulse_lag_ms` capped at 100.
    PulseStall,
}

impl AnomalyKind {
    /// Short static code for diagnostic lines.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Bottleneck => "bulkhead_starvation",
            Self::Flood => "rate_flood",
            Self::Desync => "epoch_desync",
            Self::Catastrophic => "catastrophic_fault",
            Self::PreemptStorm => "preempt_storm",
            Self::PulseStall => "pulse_stall",
        }
    }
}

/// One anomaly verdict: what, where, how bad (percent, 0..100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Anomaly {
    /// Verdict class.
    pub kind: AnomalyKind,
    /// Node that fired.
    pub node: &'static str,
    /// Guilty share in percent (catastrophic/desync carry 100/1 as markers).
    pub pct: u8,
    /// Raw guilty counter.
    pub count: u64,
}

/// Tunable anomaly thresholds. Every knob has a consumer
/// (`find_anomalies_for_snapshot`) and a test — no dead settings (R1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnomalyPolicy {
    /// Bottleneck trip percent (default 15).
    pub bottleneck_pct: u8,
    /// Flood trip percent (default 30).
    pub flood_pct: u8,
    /// Minimum attempts before percent verdicts (default 100) — avoids
    /// false positives on tiny samples. Nuclear/desync ignore it.
    pub min_sample: u64,
}

impl Default for AnomalyPolicy {
    fn default() -> Self {
        Self {
            bottleneck_pct: 15,
            flood_pct: 30,
            min_sample: 100,
        }
    }
}

impl AnomalyPolicy {
    /// Custom thresholds (clamped to 100).
    #[must_use]
    pub const fn new(bottleneck_pct: u8, flood_pct: u8, min_sample: u64) -> Self {
        Self {
            bottleneck_pct: if bottleneck_pct > 100 {
                100
            } else {
                bottleneck_pct
            },
            flood_pct: if flood_pct > 100 { 100 } else { flood_pct },
            min_sample,
        }
    }

    /// Bare-metal config proof (`no_std` invariant): percentages are
    /// already clamped by `new`, so this validates the assembled value —
    /// including `Default`. `min_sample == 0` is LEGAL (verdicts start
    /// from the first sample; the math carries `.max(1)` guards) — only
    /// `> 100` percents are poison. Firmware calls this in its own
    /// `const` block; the default policy is proven below at build time.
    pub const fn validate(&self) {
        assert!(
            self.bottleneck_pct <= 100,
            "POLICY: bottleneck_pct cannot exceed 100 (div-by-zero guard)"
        );
        assert!(
            self.flood_pct <= 100,
            "POLICY: flood_pct cannot exceed 100 (div-by-zero guard)"
        );
    }
}

// COMPILE-TIME GATE: the shipped default policy is proven sane at build.
const _: () = {
    AnomalyPolicy {
        bottleneck_pct: 15,
        flood_pct: 30,
        min_sample: 100,
    }
    .validate();
};

/// Cold anomaly scan over a snapshot (works from a decoded `app.snub`
/// with no live node at hand). Pure math: `u128` percents,
/// `total == 0` guard (no verdicts on silence), saturating everywhere.
#[must_use]
pub fn find_anomalies_for_snapshot(
    node: &'static str,
    snap: &SnapshotV1,
    policy: &AnomalyPolicy,
) -> Vec<Anomaly> {
    let mut out = Vec::new();
    let catastrophic = snap.catastrophic_faults();
    if catastrophic > 0 {
        out.push(Anomaly {
            kind: AnomalyKind::Catastrophic,
            node,
            pct: 100,
            count: catastrophic,
        });
    }
    if snap.shed_stale() > 0 {
        out.push(Anomaly {
            kind: AnomalyKind::Desync,
            node,
            pct: 1,
            count: snap.shed_stale(),
        });
    }
    // Front 4: missed heartbeats ignore `min_sample` (like desync) — a
    // single frozen pulse on an idle node is still a lag verdict.
    if snap.pulse_stalls > 0 {
        out.push(Anomaly {
            kind: AnomalyKind::PulseStall,
            node,
            pct: snap.max_pulse_lag_ms.min(100) as u8,
            count: snap.pulse_stalls,
        });
    }
    let total = snap.admission_total();
    if total >= policy.min_sample.max(1) {
        let pct_of = |n: u64| ((n as u128 * 100) / (total as u128).max(1)).min(100) as u8;
        let bulk = snap.shed_bulkhead();
        if bulk > 0 && pct_of(bulk) > policy.bottleneck_pct {
            out.push(Anomaly {
                kind: AnomalyKind::Bottleneck,
                node,
                pct: pct_of(bulk),
                count: bulk,
            });
        }
        let rate = snap.shed_rate();
        if rate > 0 && pct_of(rate) > policy.flood_pct {
            out.push(Anomaly {
                kind: AnomalyKind::Flood,
                node,
                pct: pct_of(rate),
                count: rate,
            });
        }
        let preempted = snap.shed_preempted();
        if preempted > 0 && pct_of(preempted) > policy.flood_pct {
            out.push(Anomaly {
                kind: AnomalyKind::PreemptStorm,
                node,
                pct: pct_of(preempted),
                count: preempted,
            });
        }
    }
    out
}

/// Cold anomaly scan over ONE stats snapshot. Call on a cached
/// `stats()`, never in admission.
#[cfg(feature = "std")]
#[must_use]
pub fn find_anomalies(
    node: &'static str,
    stats: &SnubberStats,
    policy: &AnomalyPolicy,
) -> Vec<Anomaly> {
    find_anomalies_for_snapshot(node, &SnapshotV1::from_stats(stats, 0), policy)
}

/// Render the 5-line pulse table into any `fmt::Write` (no alloc —
/// usable on firmware / in `no_std`). Percent math is `u128`, guarded.
pub fn write_health<W: fmt::Write>(
    w: &mut W,
    node: &str,
    snap: &SnapshotV1,
    policy: &AnomalyPolicy,
) -> fmt::Result {
    let total = snap.admission_total();
    let pct_of = |n: u64| {
        if total == 0 {
            0u8
        } else {
            ((n as u128 * 100) / (total as u128)).min(100) as u8
        }
    };
    let verdict = |n: u64, trip: u8| {
        if total < policy.min_sample.max(1) {
            "OK"
        } else if pct_of(n) > trip {
            "WARN"
        } else {
            "OK"
        }
    };
    writeln!(w, "[SNUBBER PULSE] node=\"{node}\"")?;
    writeln!(w, "CRITICAL: {} breaches", snap.catastrophic_faults())?;
    writeln!(
        w,
        "  bulk_path  | calls: {} | shed_bulk: {} ({}%) | {}",
        total,
        snap.shed_bulkhead(),
        pct_of(snap.shed_bulkhead()),
        verdict(snap.shed_bulkhead(), policy.bottleneck_pct)
    )?;
    writeln!(
        w,
        "  spam_path  | calls: {} | shed_rate: {} ({}%) | {}",
        total,
        snap.shed_rate(),
        pct_of(snap.shed_rate()),
        verdict(snap.shed_rate(), policy.flood_pct)
    )?;
    writeln!(
        w,
        "  stale_path | calls: {} | shed_stale: {} | {}",
        total,
        snap.shed_stale(),
        if snap.shed_stale() > 0 { "WARN" } else { "OK" }
    )?;
    Ok(())
}

/// Counter deltas between two snapshots of the SAME node.
///
/// `before.diff(&after)` = `after - before`, every field saturating:
/// a restarted process (generation bump, counters reset) yields zeros,
/// never underflow. Pure `Copy` math — no wire change, no IO, no alloc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SnapshotDiff {
    /// New admissions.
    pub total_started: u64,
    /// New clean commits.
    pub committed: u64,
    /// New planned aborts.
    pub explicit_aborts: u64,
    /// New implicit drops.
    pub implicit_drops: u64,
    /// New payload rejections.
    pub poisoned_rejected: u64,
    /// New stale-epoch cuts.
    pub stale_epochs_rejected: u64,
    /// New bulkhead rejections.
    pub bulkhead_rejections: u64,
    /// New evictions.
    pub evicted: u64,
    /// New preemptions.
    pub preempted: u64,
    /// New breaker fast rejections.
    pub breaker_rejections: u64,
    /// New breaker trips.
    pub breaker_trips: u64,
    /// New watchdog timeouts.
    pub timeouts_fired: u64,
    /// New rate rejections.
    pub rate_rejected: u64,
    /// New sliding-window outcomes.
    pub window_outcomes: u64,
    /// New sliding-window failures.
    pub window_failures: u64,
    /// New poison recoveries.
    pub poison_recoveries: u64,
    /// Generation delta (0 unless the process restarted).
    pub generation: u64,
    /// Per-kind report deltas ([`super::FaultKind::index`] order).
    pub reported: [u64; super::FaultKind::COUNT],
    /// New missed pulses.
    pub pulse_stalls: u64,
    /// Growth of the worst pulse lag.
    pub max_pulse_lag_ms: u64,
    /// Breaker position before the window (0 Closed, 1 Open, 2 HalfOpen).
    pub breaker_state_from: u8,
    /// Breaker position after the window.
    pub breaker_state_to: u8,
}

impl SnapshotDiff {
    /// No counter moved and the breaker position stayed put.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_started == 0
            && self.committed == 0
            && self.explicit_aborts == 0
            && self.implicit_drops == 0
            && self.poisoned_rejected == 0
            && self.stale_epochs_rejected == 0
            && self.bulkhead_rejections == 0
            && self.evicted == 0
            && self.preempted == 0
            && self.breaker_rejections == 0
            && self.breaker_trips == 0
            && self.timeouts_fired == 0
            && self.rate_rejected == 0
            && self.window_outcomes == 0
            && self.window_failures == 0
            && self.poison_recoveries == 0
            && self.generation == 0
            && self.reported.iter().all(|&v| v == 0)
            && self.pulse_stalls == 0
            && self.max_pulse_lag_ms == 0
            && self.breaker_state_from == self.breaker_state_to
    }

    /// Catastrophic deltas: panic + timeout + process-died + implicit drops.
    #[must_use]
    pub fn catastrophic_faults(&self) -> u64 {
        self.reported[super::FaultKind::Panic.index()]
            .saturating_add(self.reported[super::FaultKind::Timeout.index()])
            .saturating_add(self.reported[super::FaultKind::ProcessDied.index()])
            .saturating_add(self.implicit_drops)
    }

    /// Three ASCII lines into any `fmt::Write` (no allocation).
    pub fn write_summary(&self, w: &mut impl fmt::Write) -> fmt::Result {
        writeln!(
            w,
            "diff: admitted +{} committed +{} aborted +{} dropped +{}",
            self.total_started, self.committed, self.explicit_aborts, self.implicit_drops
        )?;
        writeln!(
            w,
            "      bulkhead +{} evicted +{} preempted +{} rate +{} stale +{}",
            self.bulkhead_rejections,
            self.evicted,
            self.preempted,
            self.rate_rejected,
            self.stale_epochs_rejected
        )?;
        writeln!(
            w,
            "      breaker {}->{} trips +{} timeouts +{} pulse +{} lag +{}ms",
            breaker_name(self.breaker_state_from),
            breaker_name(self.breaker_state_to),
            self.breaker_trips,
            self.timeouts_fired,
            self.pulse_stalls,
            self.max_pulse_lag_ms
        )
    }
}

impl SnapshotV1 {
    /// Deltas from `self` (earlier) to `later` for the same node.
    ///
    /// All subtraction is saturating, so sampling across a process
    /// restart degrades to zeros instead of wrapping.
    #[must_use]
    pub fn diff(&self, later: &Self) -> SnapshotDiff {
        let d = |before: u64, after: u64| after.saturating_sub(before);
        let mut reported = [0u64; super::FaultKind::COUNT];
        for (slot, (&a, &b)) in reported
            .iter_mut()
            .zip(self.reported.iter().zip(later.reported.iter()))
        {
            *slot = b.saturating_sub(a);
        }
        SnapshotDiff {
            total_started: d(self.total_started, later.total_started),
            committed: d(self.committed, later.committed),
            explicit_aborts: d(self.explicit_aborts, later.explicit_aborts),
            implicit_drops: d(self.implicit_drops, later.implicit_drops),
            poisoned_rejected: d(self.poisoned_rejected, later.poisoned_rejected),
            stale_epochs_rejected: d(self.stale_epochs_rejected, later.stale_epochs_rejected),
            bulkhead_rejections: d(self.bulkhead_rejections, later.bulkhead_rejections),
            evicted: d(self.evicted, later.evicted),
            preempted: d(self.preempted, later.preempted),
            breaker_rejections: d(self.breaker_rejections, later.breaker_rejections),
            breaker_trips: d(self.breaker_trips, later.breaker_trips),
            timeouts_fired: d(self.timeouts_fired, later.timeouts_fired),
            rate_rejected: d(self.rate_rejected, later.rate_rejected),
            window_outcomes: d(self.window_outcomes, later.window_outcomes),
            window_failures: d(self.window_failures, later.window_failures),
            poison_recoveries: d(self.poison_recoveries, later.poison_recoveries),
            generation: d(self.generation, later.generation),
            reported,
            pulse_stalls: d(self.pulse_stalls, later.pulse_stalls),
            max_pulse_lag_ms: d(self.max_pulse_lag_ms, later.max_pulse_lag_ms),
            breaker_state_from: self.breaker_state,
            breaker_state_to: later.breaker_state,
        }
    }
}

/// Breaker position code as text (`0/1/2` → `CLOSED/OPEN/HALF`).
#[must_use]
fn breaker_name(code: u8) -> &'static str {
    match code {
        1 => "OPEN",
        2 => "HALF",
        _ => "CLOSED",
    }
}

/// ASCII triage card for one node's cold snapshot (no allocation).
///
/// One verdict line + five counters lines, safe to `fmt::Write` into a
/// stack buffer or a log sink on a cold path. Percentages use `u128`
/// and a `total == 0` guard.
pub fn write_triage(w: &mut impl fmt::Write, node: &str, snap: &SnapshotV1) -> fmt::Result {
    let total = snap.admission_total();
    let commit_pct = if total == 0 {
        0
    } else {
        ((snap.committed as u128 * 100) / total as u128).min(100) as u8
    };
    let shed = snap
        .shed_bulkhead()
        .saturating_add(snap.shed_rate())
        .saturating_add(snap.shed_stale())
        .saturating_add(snap.shed_preempted());
    let verdict = if snap.catastrophic_faults() > 0 {
        "CRIT"
    } else if shed > 0 || snap.pulse_stalls > 0 || snap.breaker_state != 0 {
        "WARN"
    } else {
        "OK"
    };
    writeln!(w, "[SNUBBER TRIAGE] node=\"{node}\" verdict={verdict}")?;
    writeln!(w, "  breaker  {}", breaker_name(snap.breaker_state))?;
    writeln!(
        w,
        "  admits   {} (committed {} / {}%)",
        total, snap.committed, commit_pct
    )?;
    writeln!(
        w,
        "  faults   drops={} panics={} timeouts={} died={}",
        snap.implicit_drops,
        snap.reported[super::FaultKind::Panic.index()],
        snap.reported[super::FaultKind::Timeout.index()],
        snap.reported[super::FaultKind::ProcessDied.index()]
    )?;
    writeln!(
        w,
        "  shed     bulk={} evict={} rate={} stale={} preempt={}",
        snap.shed_bulkhead(),
        snap.evicted,
        snap.shed_rate(),
        snap.shed_stale(),
        snap.shed_preempted()
    )?;
    writeln!(
        w,
        "  pulse    stalls={} lag={}ms",
        snap.pulse_stalls, snap.max_pulse_lag_ms
    )
}

/// Std convenience: print the pulse table to stderr (dev hotkey /
/// telemetry console trigger). Cold path — snapshot first, render outside the lock.
#[cfg(feature = "std")]
pub fn print_health(node: &'static str, snap: &SnapshotV1, policy: &AnomalyPolicy) {
    let mut buf = StdBuf;
    let _ = write_health(&mut buf, node, snap, policy);
}

#[cfg(feature = "std")]
struct StdBuf;
#[cfg(feature = "std")]
impl fmt::Write for StdBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(s.as_bytes());
        Ok(())
    }
}

/// One-line diagnosis for bug reports, e.g.
/// `[DIAG 0x8FA3 click_worker bulkhead_starvation 92%]`.
/// Pure + `String`-free core: renders into `fmt::Write`.
pub fn write_diagnostic_line<W: fmt::Write>(
    w: &mut W,
    node: &'static str,
    anomaly: &Anomaly,
    snap: &SnapshotV1,
) -> fmt::Result {
    write!(
        w,
        "[DIAG 0x{:04X} {} {} {}%]",
        diagnostic_hash(node, snap) & 0xFFFF,
        node,
        anomaly.kind.code(),
        anomaly.pct
    )
}

/// Std convenience: the diagnostic line as a `String` (issue text).
#[cfg(feature = "std")]
#[must_use]
pub fn diagnostic_line(node: &'static str, anomaly: &Anomaly, snap: &SnapshotV1) -> String {
    format!(
        "[DIAG 0x{:04X} {} {} {}%]",
        diagnostic_hash(node, snap) & 0xFFFF,
        node,
        anomaly.kind.code(),
        anomaly.pct
    )
}

/// Borrowed in-memory node aggregate (no I/O, no threads, no paths).
///
/// The app owns it (`let set = NodeSet::new(); set.register(..)`),
/// polls cold snapshots, scans anomalies, sums counters with `saturating_add`.
#[cfg(feature = "std")]
#[derive(Default)]
pub struct NodeSet<'a> {
    nodes: Vec<(&'static str, &'a SnubberNode)>,
}

#[cfg(feature = "std")]
impl<'a> NodeSet<'a> {
    /// Empty set.
    #[must_use]
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Register one node handle (explicit, reversible via `clear`).
    pub fn register(&mut self, name: &'static str, node: &'a SnubberNode) {
        self.nodes.push((name, node));
    }

    /// Forget all handles (nodes keep living; only the set clears).
    pub fn clear(&mut self) {
        self.nodes.clear();
    }

    /// Handles in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// `true` when no handles registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Cold snapshots of every member node (oldest registration first).
    #[must_use]
    pub fn snapshots(&self) -> Vec<(&'static str, SnapshotV1)> {
        self.nodes
            .iter()
            .map(|(name, node)| (*name, node.snapshot_v1()))
            .collect()
    }

    /// Anomaly scan across the whole set (cold, pure math).
    #[must_use]
    pub fn find_all_anomalies(&self, policy: &AnomalyPolicy) -> Vec<Anomaly> {
        let mut out = Vec::new();
        for (name, node) in &self.nodes {
            out.extend(find_anomalies(name, &node.stats(), policy));
        }
        out
    }

    /// Sum of `catastrophic_faults()` over the set (saturating).
    #[must_use]
    pub fn total_faults(&self) -> u64 {
        self.nodes
            .iter()
            .map(|(_, n)| n.stats().catastrophic_faults())
            .fold(0u64, |a, b| a.saturating_add(b))
    }

    /// Catastrophic fault sum (same ledger as [`total_faults`](Self::total_faults);
    /// kept as the spec-name alias so monitors read intent, not arithmetic).
    #[must_use]
    pub fn total_catastrophic_faults(&self) -> u64 {
        self.total_faults()
    }

    /// Hydraulic shedding triple `(bulkhead, rate, stale)` summed over the set.
    #[must_use]
    pub fn total_hydraulic_shed(&self) -> (u64, u64, u64) {
        let mut bulk = 0u64;
        let mut rate = 0u64;
        let mut stale = 0u64;
        for (_, n) in &self.nodes {
            let s = n.stats();
            bulk = bulk.saturating_add(s.shed_bulkhead());
            rate = rate.saturating_add(s.shed_rate());
            stale = stale.saturating_add(s.shed_stale());
        }
        (bulk, rate, stale)
    }

    /// `true` when any member breaker is not `Closed`.
    #[must_use]
    pub fn is_any_breaker_tripped(&self) -> bool {
        self.nodes
            .iter()
            .any(|(_, n)| n.stats().breaker_state != super::BreakerState::Closed)
    }

    /// One summary line per node into `sink` (cold path only).
    pub fn dump_summary<F: FnMut(&str)>(&self, mut sink: F) {
        for (name, node) in &self.nodes {
            let s = node.stats();
            // Fixed-width rendering without `format!` on the hot path:
            // this runs on the cold diagnostic path only.
            let line = format!(
                "{} started={} committed={} aborts={} implicit={} catastrophic={} breaker={:?}",
                name,
                s.total_started,
                s.committed,
                s.explicit_aborts,
                s.implicit_drops,
                s.catastrophic_faults(),
                s.breaker_state,
            );
            sink(&line);
        }
    }
}

/// Test/CI gate: fail red when the node sheds or breaches past policy.
///
/// ```rust,no_run
/// use snubber::{AnomalyPolicy, SnubberNode, assert_no_anomalies};
/// let node = SnubberNode::builder("click_worker").build();
/// assert_no_anomalies!(&node);
/// ```
#[cfg(feature = "std")]
#[macro_export]
macro_rules! assert_no_anomalies {
    ($node:expr) => {
        $crate::assert_no_anomalies!($node, &$crate::AnomalyPolicy::default())
    };
    ($node:expr, $policy:expr) => {{
        let __stats = $node.stats();
        let __name = $node.name();
        let __hits = $crate::find_anomalies(__name, &__stats, $policy);
        assert!(
            __hits.is_empty(),
            "snubber anomalies on node {:?}: {:?} (stats: {:?})",
            __name,
            __hits,
            __stats
        );
    }};
}
