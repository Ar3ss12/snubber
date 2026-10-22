//! Aggregated pool report: one row per lineage plus conservation totals.

use std::fmt;

use crate::memory::MemoryWatch;

use super::ReapClass;

/// One lane's row in a [`PoolReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSnapshot {
    /// Lane index.
    pub index: usize,
    /// Current (successor) generation.
    pub generation: u64,
    /// Attached child pid (`None` = idle).
    pub pid: Option<u32>,
    /// Reaps reported through [`note_death`](super::PoolLane::note_death).
    pub reaps: u64,
    /// Reaps that handed back an un-settled token.
    pub rescued: u64,
    /// [`ReapClass::Ambush`] count.
    pub ambush: u64,
    /// [`ReapClass::Escape`] count.
    pub escape: u64,
    /// [`ReapClass::Stillborn`] count.
    pub stillborn: u64,
    /// [`ReapClass::Deadlocked`] count (must stay `0` in a healthy run).
    pub deadlocks: u64,
    /// Death reports seen by this lane's tracker.
    pub attempts: u32,
    /// Restarts authorized for this lane.
    pub restarts: u32,
    /// Storm-guard trips for this lane.
    pub giveups: u32,
    /// Last observed exit code (`None` = killed/hung from outside).
    pub last_exit: Option<i32>,
    /// Classification of the last reap.
    pub last_class: Option<ReapClass>,
    /// This lane's memory watch (last sample, peak, limits).
    pub memory: MemoryWatch,
}

impl LaneSnapshot {
    /// `ambush + escape + stillborn + deadlocks == reaps`.
    ///
    /// `deadlocks` is a fourth honest bucket: a reap whose pipe never
    /// reached EOF is NOT guessed into any of the three classes.
    #[must_use]
    pub fn conserved(&self) -> bool {
        self.ambush
            .saturating_add(self.escape)
            .saturating_add(self.stillborn)
            .saturating_add(self.deadlocks)
            == self.reaps
    }
}

/// Pool-wide sums over every [`LaneSnapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolTotals {
    /// Lane count.
    pub lanes: usize,
    /// Reaps across all lanes.
    pub reaps: u64,
    /// Rescued tokens across all lanes.
    pub rescued: u64,
    /// Ambush reaps across all lanes.
    pub ambush: u64,
    /// Escape reaps across all lanes.
    pub escape: u64,
    /// Stillborn reaps across all lanes.
    pub stillborn: u64,
    /// Deadlocked reaps across all lanes.
    pub deadlocks: u64,
    /// Authorized restarts across all lanes.
    pub restarts: u64,
    /// Storm-guard trips across all lanes.
    pub giveups: u64,
}

impl PoolTotals {
    /// Sum the rows.
    #[must_use]
    pub fn of(lanes: &[LaneSnapshot]) -> Self {
        let mut totals = Self {
            lanes: lanes.len(),
            ..Self::default()
        };
        for lane in lanes {
            totals.reaps = totals.reaps.saturating_add(lane.reaps);
            totals.rescued = totals.rescued.saturating_add(lane.rescued);
            totals.ambush = totals.ambush.saturating_add(lane.ambush);
            totals.escape = totals.escape.saturating_add(lane.escape);
            totals.stillborn = totals.stillborn.saturating_add(lane.stillborn);
            totals.deadlocks = totals.deadlocks.saturating_add(lane.deadlocks);
            totals.restarts = totals.restarts.saturating_add(u64::from(lane.restarts));
            totals.giveups = totals.giveups.saturating_add(u64::from(lane.giveups));
        }
        totals
    }

    /// `ambush + escape + stillborn + deadlocks == reaps`.
    #[must_use]
    pub fn conserved(&self) -> bool {
        self.ambush
            .saturating_add(self.escape)
            .saturating_add(self.stillborn)
            .saturating_add(self.deadlocks)
            == self.reaps
    }
}

/// Aggregate snapshot of a [`WorkerPool`](super::WorkerPool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolReport {
    /// Per-lane rows in index order.
    pub lanes: Vec<LaneSnapshot>,
    /// Pool-wide sums.
    pub totals: PoolTotals,
}

impl PoolReport {
    /// Build a report from lane rows.
    #[must_use]
    pub fn new(lanes: Vec<LaneSnapshot>) -> Self {
        let totals = PoolTotals::of(&lanes);
        Self { lanes, totals }
    }

    /// Every lane (and the totals) conserves its reaps.
    #[must_use]
    pub fn conserved(&self) -> bool {
        self.lanes.iter().all(LaneSnapshot::conserved) && self.totals.conserved()
    }

    /// At least one reap could not verify its pipe.
    #[must_use]
    pub fn deadlocked(&self) -> bool {
        self.totals.deadlocks > 0
    }

    /// Conserved AND no deadlocks: the pool's accounting is trustworthy.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.conserved() && !self.deadlocked()
    }

    /// Row by lane index.
    #[must_use]
    pub fn lane(&self, index: usize) -> Option<&LaneSnapshot> {
        self.lanes.get(index)
    }

    /// Current generation of every lane, in index order.
    #[must_use]
    pub fn final_generations(&self) -> Vec<u64> {
        self.lanes.iter().map(|lane| lane.generation).collect()
    }
}

impl fmt::Display for PoolReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let totals = &self.totals;
        writeln!(
            f,
            "POOL lanes={} reaps={} rescued={} ambush={} escape={} stillborn={} deadlocks={} conserved={}",
            totals.lanes,
            totals.reaps,
            totals.rescued,
            totals.ambush,
            totals.escape,
            totals.stillborn,
            totals.deadlocks,
            self.conserved()
        )?;
        for lane in &self.lanes {
            let pid = match lane.pid {
                Some(pid) => pid.to_string(),
                None => "-".to_string(),
            };
            writeln!(
                f,
                "  lane={} gen={} pid={} reaps={} rescued={} A/E/S/D={}/{}/{}/{} restarts={} giveups={} mem={}/{}kB",
                lane.index,
                lane.generation,
                pid,
                lane.reaps,
                lane.rescued,
                lane.ambush,
                lane.escape,
                lane.stillborn,
                lane.deadlocks,
                lane.restarts,
                lane.giveups,
                lane.memory.used_kb(),
                lane.memory.peak_kb()
            )?;
        }
        Ok(())
    }
}
