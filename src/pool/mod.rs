//! Worker pool: N independent supervised lineages, one pipe per worker.
//!
//! [`Supervisor`](crate::Supervisor) owns exactly ONE lineage: one armed
//! slot in its [`PipeDrain`], one generation clock, one blocking
//! `child.wait()`. Putting eight workers behind one supervisor would merge
//! their accounting — a rescue token from lane 3 would be indistinguishable
//! from lane 5, and one silent worker would freeze every other lane.
//!
//! This module keeps the wire format, the drain semantics and the
//! single-lineage [`Supervisor`](crate::Supervisor) untouched and scales the
//! COUNT instead:
//!
//! * lane `i` = one [`GenerationClock`](crate::GenerationClock), one
//!   [`RestartTracker`](crate::RestartTracker) (+ backoff + storm window),
//!   one [`MemoryPolicy`], one [`PipeDrain`], one pid — see [`PoolLane`];
//! * `N` app threads, each blocking on its own child and draining its own
//!   pipe; a lane guards only its own state, so lanes never contend;
//! * the APP still owns `spawn`, `kill`, `sleep` and pid files; this module
//!   owns the rescue classification, the restart math and the aggregated
//!   [`PoolReport`].
//!
//! Task ids can be lane-namespaced ([`namespace_task`], `task = lane << 48 |
//! local`) so a rescued token always names its lineage.
//!
//! ```rust
//! use snubber::pool::{PoolConfig, ReapClass, WorkerPool};
//! use snubber::{PipeDrain, ProcessToken};
//! use std::io::Cursor;
//!
//! let pool = WorkerPool::new(PoolConfig::new(2)).expect("two lanes");
//! let lane = pool.lane(0).expect("lane 0");
//! let armed = ProcessToken::simple(7, 9, 1).to_bytes().to_vec();
//! lane.attach(4_242, PipeDrain::drain_sync(Cursor::new(armed)))
//!     .expect("fresh lane");
//! let reap = lane.note_death(None, 1_000).expect("attached child");
//! assert_eq!(reap.class, ReapClass::Ambush);
//! assert_eq!(reap.token.map(|t| t.task), Some(7));
//! assert!(pool.report().conserved());
//! ```

pub mod lane;
pub mod report;
pub mod task;

pub use lane::{PoolLane, PoolReap, ReapClass};
pub use report::{LaneSnapshot, PoolReport, PoolTotals};
pub use task::{namespace_task, task_lane, task_local, LANE_SHIFT, MAX_LANES, TASK_LOCAL_MASK};

use std::sync::Arc;

use crate::supervisor::{Backoff, MemoryPolicy, RestartPolicy, RestartWindow};

/// Default EOF grace before a silent drain is reaped as
/// [`ReapClass::Deadlocked`], ms.
///
/// FORMAT CONTRACT: 250 ms is the published default that the pool tests
/// and the supervisor examples assume; it is overridable per pool via
/// [`PoolConfig::with_eof_grace`], never silently changed.
pub const DEFAULT_EOF_GRACE_MS: u64 = 250;

/// Declarative pool configuration, copied into every lane.
///
/// The restart parts are PER LANE: lane 3's crash storm advances only lane
/// 3's window and backoff. Memory limits are per lane too — the app samples
/// each child separately and hands the value to that lane's
/// [`PoolLane::note_memory_kb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolConfig {
    /// Lane count (`1..=MAX_LANES`); validated by [`WorkerPool::new`].
    pub lanes: usize,
    /// Restart policy applied independently to every lane.
    pub policy: RestartPolicy,
    /// Per-lane cooling math (the app sleeps, the crate never does).
    pub backoff: Backoff,
    /// Per-lane storm window.
    pub window: RestartWindow,
    /// Per-lane soft/hard limits (`0` disables; see [`MemoryPolicy`]).
    pub memory: MemoryPolicy,
    /// Maximum wait for drain EOF before a reap is reported as
    /// [`ReapClass::Deadlocked`], ms.
    ///
    /// `0` disables the window entirely: the drain is polled once
    /// without waiting, and a pipe that has not reached EOF at that
    /// instant is reaped as Deadlocked (an existing, tested contract —
    /// use it only when a misclassification is cheaper than the wait).
    /// `u64::MAX` waits for EOF unboundedly (no Deadlocked verdict).
    pub eof_grace_ms: u64,
}

impl PoolConfig {
    /// Defaults: `OnCrash`, `50 → 1000 ms` backoff, `3 per 10 s` window,
    /// memory limits off, `250 ms` EOF grace.
    #[must_use]
    pub const fn new(lanes: usize) -> Self {
        Self {
            lanes,
            policy: RestartPolicy::OnCrash,
            backoff: Backoff::exponential(50, 1000),
            window: RestartWindow::new(3, 10),
            memory: MemoryPolicy::new(0, 0),
            eof_grace_ms: DEFAULT_EOF_GRACE_MS,
        }
    }

    /// Replace the per-lane restart parts.
    #[must_use]
    pub const fn with_restart(
        mut self,
        policy: RestartPolicy,
        backoff: Backoff,
        window: RestartWindow,
    ) -> Self {
        self.policy = policy;
        self.backoff = backoff;
        self.window = window;
        self
    }

    /// Set the per-lane memory limits (`0` disables either).
    #[must_use]
    pub const fn with_memory(mut self, soft_kb: u64, hard_kb: u64) -> Self {
        self.memory = MemoryPolicy::new(soft_kb, hard_kb);
        self
    }

    /// Set the EOF grace window used by [`PoolLane::note_death`].
    ///
    /// `0` polls EOF once with no wait (instant `Deadlocked` on a
    /// non-EOF drain); `u64::MAX` waits unboundedly.
    #[must_use]
    pub const fn with_eof_grace(mut self, eof_grace_ms: u64) -> Self {
        self.eof_grace_ms = eof_grace_ms;
        self
    }
}

impl Default for PoolConfig {
    #[inline]
    fn default() -> Self {
        Self::new(4)
    }
}

/// Pool construction refusal. Lane-level refusals are listed last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PoolError {
    /// Zero lanes requested.
    NoLanes,
    /// More lanes than the 16-bit lane field can address.
    TooManyLanes {
        /// Requested lane count.
        requested: usize,
        /// Addressable maximum ([`MAX_LANES`]).
        max: usize,
    },
    /// [`PoolLane::attach`] on a lane that still holds a live child.
    LaneBusy {
        /// Lane index.
        lane: usize,
    },
    /// A reap or memory sample on a lane with no attached child.
    NoWorkerAttached {
        /// Lane index.
        lane: usize,
    },
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::NoLanes => write!(f, "pool needs at least one lane"),
            Self::TooManyLanes { requested, max } => {
                write!(f, "pool requested {requested} lanes, max is {max}")
            }
            Self::LaneBusy { lane } => {
                write!(f, "lane {lane} already has an attached worker")
            }
            Self::NoWorkerAttached { lane } => {
                write!(f, "lane {lane} has no attached worker")
            }
        }
    }
}

impl std::error::Error for PoolError {}

/// N independent lanes; the app drives one thread per lane.
///
/// Construction validates the lane count once. The pool itself is `Send +
/// Sync` and cheap to share: lanes carry interior mutability, so each
/// waiter thread locks only its own lane.
#[derive(Debug, Clone)]
pub struct WorkerPool {
    lanes: Vec<Arc<PoolLane>>,
    config: PoolConfig,
}

impl WorkerPool {
    /// Build `config.lanes` fresh lanes (generation 1, no child attached).
    ///
    /// # Errors
    /// [`PoolError::NoLanes`] / [`PoolError::TooManyLanes`].
    pub fn new(config: PoolConfig) -> Result<Self, PoolError> {
        if config.lanes == 0 {
            return Err(PoolError::NoLanes);
        }
        if config.lanes > MAX_LANES {
            return Err(PoolError::TooManyLanes {
                requested: config.lanes,
                max: MAX_LANES,
            });
        }
        let lanes = (0..config.lanes)
            .map(|index| Arc::new(PoolLane::new(index, &config)))
            .collect();
        Ok(Self { lanes, config })
    }

    /// Configuration this pool was built from.
    #[must_use]
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Number of lanes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lanes.len()
    }

    /// `true` when the pool has no lanes (never: construction refuses it).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    /// Lane by index.
    #[must_use]
    pub fn lane(&self, index: usize) -> Option<&Arc<PoolLane>> {
        self.lanes.get(index)
    }

    /// All lanes in index order.
    #[must_use]
    pub fn lanes(&self) -> &[Arc<PoolLane>] {
        &self.lanes
    }

    /// Aggregate snapshot: one row per lane plus totals.
    #[must_use]
    pub fn report(&self) -> PoolReport {
        PoolReport::new(self.lanes.iter().map(|lane| lane.snapshot()).collect())
    }

    /// Lanes whose pipe proved traffic once and then went silent past
    /// `lease_ms`, as `(lane index, idle ms)`.
    ///
    /// One app monitor thread calls this on a tick while the lane waiters
    /// stay blocked on `child.wait()`: every returned lane is a candidate
    /// for the app's own `kill()` + [`PoolLane::note_death`]. A lane that
    /// never parsed a frame is NOT returned — a child hung before its
    /// first heartbeat is the app's spawn timeout, not a lease breach.
    #[must_use]
    pub fn expired_lanes(&self, lease_ms: u64) -> Vec<(usize, u64)> {
        let mut expired = Vec::new();
        for lane in &self.lanes {
            if lane.expired(lease_ms) {
                expired.push((lane.index(), lane.idle_ms()));
            }
        }
        expired
    }

    /// Rescued tokens summed across lanes.
    #[must_use]
    pub fn total_rescued(&self) -> u64 {
        self.lanes
            .iter()
            .map(|lane| lane.snapshot().rescued)
            .fold(0u64, u64::saturating_add)
    }
}

// COMPILE-TIME GATES (pool geometry, always compiled):
// the lane field and the local task field must tile a `u64` exactly.
const _: () = {
    assert!(
        LANE_SHIFT == 48,
        "CRITICAL: lane id must own the high 16 bits of a task id"
    );
    assert!(
        TASK_LOCAL_MASK == u64::MAX >> 16,
        "CRITICAL: local task id must be the low 48 bits"
    );
    assert!(
        MAX_LANES == 1 << 16,
        "CRITICAL: MAX_LANES must match the 16-bit lane field"
    );
};
