//! Lane-namespaced task ids: `task = lane << 48 | local`.
//!
//! A rescued token carries only `(task, opcode, payload)`. When several
//! lineages share a log or a rollback table, the task id must name the lane:
//!
//! ```text
//! 63        48 47                    0
//! +-----------+----------------------+
//! | lane (16) | local task id (48)   |
//! +-----------+----------------------+
//! ```
//!
//! `namespace_task` masks the local id to 48 bits, so packing cannot spill
//! into the lane field. Ids minted by [`namespace_task`] are reversible with
//! [`task_lane`] / [`task_local`]; plain (un-namespaced) ids keep working —
//! the split is an app-level convention, not a wire change.

/// Bit offset of the lane field inside a task id.
pub const LANE_SHIFT: u32 = 48;

/// Mask of the local (per-lane) task id.
pub const TASK_LOCAL_MASK: u64 = (1u64 << LANE_SHIFT) - 1;

/// Addressable lane count (`2^16`).
pub const MAX_LANES: usize = 1 << 16;

/// Pack `local` into `lane`'s namespace (local id masked to 48 bits).
#[must_use]
pub const fn namespace_task(lane: u16, local: u64) -> u64 {
    ((lane as u64) << LANE_SHIFT) | (local & TASK_LOCAL_MASK)
}

/// Lane field of a task id (`0` for un-namespaced ids below `2^48`).
#[must_use]
pub const fn task_lane(task: u64) -> u16 {
    (task >> LANE_SHIFT) as u16
}

/// Local task id with the lane field stripped.
#[must_use]
pub const fn task_local(task: u64) -> u64 {
    task & TASK_LOCAL_MASK
}

const _: () = {
    assert!(
        LANE_SHIFT + 16 == 64,
        "CRITICAL: lane field must exactly fill the high 16 bits"
    );
    assert!(
        TASK_LOCAL_MASK == u64::MAX >> 16,
        "CRITICAL: local task id must be the low 48 bits"
    );
    assert!(
        MAX_LANES == 1usize << 16,
        "CRITICAL: MAX_LANES must match the 16-bit lane field"
    );
};
