//! One pool lane: the state that belongs to exactly ONE supervised child.
//!
//! A lane owns its drain, so the single `armed` slot, the single `OP_STATE`
//! scratch slot and `take_armed`'s exactly-once contract stay meaningful —
//! they now describe one worker instead of a merged crowd.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::memory::MemoryVerdict;
use crate::supervisor::{
    DrainCounters, GenerationClock, MemoryPolicy, PipeDrain, ProcessToken, RestartDecision,
    RestartTracker, StateDigest,
};

use super::report::LaneSnapshot;
use super::{PoolConfig, PoolError};

/// How a reaped child is classified from ITS OWN drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReapClass {
    /// An un-settled arm frame was still in the pipe at death.
    Ambush,
    /// At least one `OP_COMMIT` was seen and no arm is left un-settled.
    Escape,
    /// Neither an arm nor a commit: the worker died before settling anything.
    Stillborn,
    /// The drain did not reach EOF inside the grace window. Nothing is
    /// claimed and no class is guessed; the app must treat the lane as
    /// unverifiable (a healthy storm reports zero of these).
    Deadlocked,
}

/// One reap: classification, rescued token and the per-lane restart order.
#[must_use = "a PoolReap carries the rescued token and the restart decision — match or forward it, never drop it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolReap {
    /// Lane index this child belonged to.
    pub lane: usize,
    /// Generation for the SUCCESSOR child (the lane clock was bumped once).
    pub generation: u64,
    /// Exit code as observed by the app (`None` = killed/hung from outside).
    pub exit_code: Option<i32>,
    /// Rescue classification.
    pub class: ReapClass,
    /// Un-settled token (`Some` only for [`ReapClass::Ambush`]); the APP runs
    /// the rollback, the crate only hands the bytes back.
    pub token: Option<ProcessToken>,
    /// Last verified `StateDigest` offered by this child (handoff payload).
    pub state: Option<StateDigest>,
    /// Frame counters observed by this child's drain.
    pub counters: DrainCounters,
    /// `true` when EOF was not observed inside the grace window.
    pub deadlock: bool,
    /// Per-lane restart order (the app owns sleep + spawn).
    pub decision: RestartDecision,
}

impl PoolReap {
    /// Cooling pause when the lane is allowed to respawn, ms.
    #[must_use]
    pub const fn respawn_delay_ms(&self) -> Option<u64> {
        match self.decision {
            RestartDecision::Restart { delay_ms, .. } => Some(delay_ms),
            RestartDecision::Stop | RestartDecision::GiveUp { .. } => None,
        }
    }
}

#[derive(Debug)]
struct LaneState {
    clock: GenerationClock,
    tracker: RestartTracker,
    memory: MemoryPolicy,
    pid: Option<u32>,
    drain: Option<PipeDrain>,
    reaps: u64,
    rescued: u64,
    ambush: u64,
    escape: u64,
    stillborn: u64,
    deadlocks: u64,
    last_exit: Option<i32>,
    last_class: Option<ReapClass>,
}

/// One independently supervised lineage.
///
/// Share a lane with exactly one app thread: that thread spawns, waits,
/// kills and reports the child. Every accessor locks only this lane.
#[derive(Debug)]
pub struct PoolLane {
    index: usize,
    eof_grace_ms: u64,
    base_memory: MemoryPolicy,
    state: Mutex<LaneState>,
}

impl PoolLane {
    pub(crate) fn new(index: usize, config: &PoolConfig) -> Self {
        Self {
            index,
            eof_grace_ms: config.eof_grace_ms,
            base_memory: config.memory,
            state: Mutex::new(LaneState {
                clock: GenerationClock::new(),
                tracker: RestartTracker::new(config.policy, config.backoff, config.window),
                memory: config.memory,
                pid: None,
                drain: None,
                reaps: 0,
                rescued: 0,
                ambush: 0,
                escape: 0,
                stillborn: 0,
                deadlocks: 0,
                last_exit: None,
                last_class: None,
            }),
        }
    }

    /// Poison-tolerant lock: lane state is plain data, a panic elsewhere
    /// must not park the remaining lanes.
    fn state(&self) -> MutexGuard<'_, LaneState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lane index, stamped on every [`PoolReap`] and report row.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// Generation the NEXT child of this lane must carry.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.state().clock.generation()
    }

    /// Next monotonic epoch inside the current generation (handshake value).
    pub fn next_epoch(&self) -> u64 {
        self.state().clock.alloc_epoch()
    }

    /// Attach a freshly spawned child and its stdout drain.
    ///
    /// Returns the generation to stamp on the child (`SNUBBER_GEN` or the
    /// app's own envelope). The per-lane memory watch resets here, so a
    /// previous child's peak can never euthanize its successor.
    ///
    /// # Errors
    /// [`PoolError::LaneBusy`] if a child is still attached.
    pub fn attach(&self, pid: u32, drain: PipeDrain) -> Result<u64, PoolError> {
        let mut state = self.state();
        if state.pid.is_some() {
            return Err(PoolError::LaneBusy { lane: self.index });
        }
        state.pid = Some(pid);
        state.drain = Some(drain);
        state.memory = self.base_memory;
        state.last_class = None;
        Ok(state.clock.generation())
    }

    /// Attached child pid (`None` = idle, awaiting respawn).
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.state().pid
    }

    /// Parsed-frame counters of the attached drain (zeroed when idle).
    #[must_use]
    pub fn counters(&self) -> DrainCounters {
        let state = self.state();
        state
            .drain
            .as_ref()
            .map_or_else(DrainCounters::default, PipeDrain::counters)
    }

    /// Milliseconds since the attached child's last valid frame (`0` when idle).
    #[must_use]
    pub fn idle_ms(&self) -> u64 {
        let state = self.state();
        state.drain.as_ref().map_or(0, PipeDrain::idle_ms)
    }

    /// `true` when this lane's pipe is alive but silent past `lease_ms`.
    #[must_use]
    pub fn expired(&self, lease_ms: u64) -> bool {
        let state = self.state();
        state
            .drain
            .as_ref()
            .is_some_and(|drain| drain.expired(lease_ms))
    }

    /// Record one external RSS sample for the attached child.
    ///
    /// # Errors
    /// [`PoolError::NoWorkerAttached`] when the lane is idle.
    pub fn note_memory_kb(&self, used_kb: u64) -> Result<MemoryVerdict, PoolError> {
        let mut state = self.state();
        if state.pid.is_none() {
            return Err(PoolError::NoWorkerAttached { lane: self.index });
        }
        Ok(state.memory.note(used_kb))
    }

    /// Verdict for this lane's last memory sample.
    #[must_use]
    pub fn memory_verdict(&self) -> MemoryVerdict {
        self.state().memory.verdict()
    }

    /// Reap the attached child and classify the rescue from ITS pipe.
    ///
    /// 1. waits up to `eof_grace_ms` for drain EOF (outside the lane lock);
    /// 2. EOF + un-settled arm → [`ReapClass::Ambush`] + token handed back;
    /// 3. EOF + commits only → [`ReapClass::Escape`];
    /// 4. EOF + silence → [`ReapClass::Stillborn`];
    /// 5. no EOF inside the grace → [`ReapClass::Deadlocked`], token `None`.
    ///
    /// The lane generation is bumped exactly once per reported death, so
    /// stale packets from the dead child never reach its successor. The
    /// restart decision is per lane: another lane's crash storm cannot
    /// stop this lineage. `now_ms` is the caller's monotonic clock and
    /// feeds only the tracker (tests inject a fake clock).
    ///
    /// # Errors
    /// [`PoolError::NoWorkerAttached`] if the lane has no child to reap
    /// (a double report is a caller bug, not a death).
    pub fn note_death(&self, exit_code: Option<i32>, now_ms: u64) -> Result<PoolReap, PoolError> {
        let drain = {
            let mut state = self.state();
            let drain = state
                .drain
                .take()
                .ok_or(PoolError::NoWorkerAttached { lane: self.index })?;
            state.pid = None;
            drain
        };

        let mut deadlock = false;
        if self.eof_grace_ms > 0 {
            // Grace runs on the drain's own clock: the default is the
            // process monotonic ms, tests inject a counter, firmware pushes
            // chunks and calls `finish()` from its own task.
            // Strictly greater: millisecond clocks truncate, so `>=` can
            // declare the grace over up to 1 ms early. `>` costs one poll
            // and guarantees the caller actually got the full window.
            let started = drain.now_ms();
            while !drain.eof() {
                if drain.now_ms().saturating_sub(started) > self.eof_grace_ms {
                    deadlock = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        } else {
            deadlock = !drain.eof();
        }

        let counters = drain.counters();
        let token = if deadlock { None } else { drain.take_armed() };
        let state_digest = drain.take_state();
        let class = if deadlock {
            ReapClass::Deadlocked
        } else if token.is_some() {
            ReapClass::Ambush
        } else if counters.commits > 0 {
            ReapClass::Escape
        } else {
            ReapClass::Stillborn
        };

        let mut state = self.state();
        let generation = state.clock.restart();
        let decision = state.tracker.note_death(exit_code, now_ms);
        state.reaps = state.reaps.saturating_add(1);
        match class {
            ReapClass::Ambush => {
                state.ambush = state.ambush.saturating_add(1);
                state.rescued = state.rescued.saturating_add(1);
            }
            ReapClass::Escape => state.escape = state.escape.saturating_add(1),
            ReapClass::Stillborn => state.stillborn = state.stillborn.saturating_add(1),
            ReapClass::Deadlocked => state.deadlocks = state.deadlocks.saturating_add(1),
        }
        state.last_exit = exit_code;
        state.last_class = Some(class);
        drop(state);

        Ok(PoolReap {
            lane: self.index,
            generation,
            exit_code,
            class,
            token,
            state: state_digest,
            counters,
            deadlock,
            decision,
        })
    }

    /// One report row (short lock, plain integers).
    #[must_use]
    pub fn snapshot(&self) -> LaneSnapshot {
        let state = self.state();
        LaneSnapshot {
            index: self.index,
            generation: state.clock.generation(),
            pid: state.pid,
            reaps: state.reaps,
            rescued: state.rescued,
            ambush: state.ambush,
            escape: state.escape,
            stillborn: state.stillborn,
            deadlocks: state.deadlocks,
            attempts: state.tracker.attempts(),
            restarts: state.tracker.restarts(),
            giveups: state.tracker.giveups(),
            last_exit: state.last_exit,
            last_class: state.last_class,
            memory: state.memory.watch(),
        }
    }
}
