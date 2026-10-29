//! Teardown order as a type parameter.
//!
//! A stop loop that walks nodes in registration order is an accident waiting
//! to happen: stop the engine before the sink it reports into and the last
//! frames are lost exactly when they matter. [`RankedNode`] pins a node's
//! position with `const RANK: u8`, and [`RankedSet`] keeps entries in that
//! order from the moment they are inserted, so the loop reads a fixed array
//! front to back — no sort at read time, no heap, no allocation.
//!
//! Higher `RANK` is stopped first. The rank is a type parameter rather than a
//! constructor argument because a node's position is a property of the
//! topology, not of one call site: `RankedNode::<3>::new(&log_sink)` compiles
//! the same everywhere, while a `rank: u8` argument is a number any caller
//! can get wrong and no test can see. [`RankedSet`] stores it as data anyway,
//! because one set holds entries of many different `RANK` types.

#[cfg(feature = "std")]
use core::fmt;

#[cfg(feature = "std")]
use crate::{NodeName, SnapshotV1, SnubberNode, SnubberStats};

/// One node with a compile-time stop order (higher `RANK` stops first).
#[derive(Clone, Copy)]
pub struct RankedNode<'a, const RANK: u8> {
    node: &'a SnubberNode,
}

// `SnubberNode` is not `Debug` (its counters are read through `stats()`), so
// the wrapper prints the label and the rank instead of the node's guts.
impl<const RANK: u8> fmt::Debug for RankedNode<'_, RANK> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RankedNode")
            .field("rank", &RANK)
            .field("node", &self.node.name().as_str())
            .finish()
    }
}

impl<'a, const RANK: u8> RankedNode<'a, RANK> {
    /// Wrap a node at the rank its declaration names.
    #[must_use]
    pub const fn new(node: &'a SnubberNode) -> Self {
        Self { node }
    }

    /// Stop order carried by the type (`RANK`, never a stored field).
    #[must_use]
    pub const fn rank(&self) -> u8 {
        RANK
    }

    /// The wrapped node.
    #[must_use]
    pub const fn node(&self) -> &'a SnubberNode {
        self.node
    }

    /// Node identity, borrowed from the node itself so the two cannot drift.
    #[must_use]
    pub fn name(&self) -> NodeName {
        self.node.name()
    }

    /// Cold snapshot of the wrapped node.
    #[must_use]
    pub fn snapshot_v1(&self) -> SnapshotV1 {
        self.node.snapshot_v1()
    }

    /// Hot-path counters of the wrapped node.
    #[must_use]
    pub fn stats(&self) -> SnubberStats {
        self.node.stats()
    }
}

/// One set member: a node plus the rank its type carried on the way in.
///
/// `Copy`, four words wide — the set is a fixed array of these, so ordered
/// iteration borrows entries, never clones them.
#[derive(Clone, Copy)]
pub struct RankedEntry<'a> {
    rank: u8,
    node: &'a SnubberNode,
}

impl fmt::Debug for RankedEntry<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RankedEntry")
            .field("rank", &self.rank)
            .field("node", &self.node.name().as_str())
            .finish()
    }
}

impl<'a> RankedEntry<'a> {
    /// Stop order this entry was admitted with.
    #[must_use]
    pub const fn rank(&self) -> u8 {
        self.rank
    }

    /// The member node.
    #[must_use]
    pub const fn node(&self) -> &'a SnubberNode {
        self.node
    }

    /// Node identity, borrowed from the node itself.
    #[must_use]
    pub fn name(&self) -> NodeName {
        self.node.name()
    }

    /// Cold snapshot of the member node.
    #[must_use]
    pub fn snapshot_v1(&self) -> SnapshotV1 {
        self.node.snapshot_v1()
    }

    /// Hot-path counters of the member node.
    #[must_use]
    pub fn stats(&self) -> SnubberStats {
        self.node.stats()
    }
}

impl<'a, const RANK: u8> From<RankedNode<'a, RANK>> for RankedEntry<'a> {
    fn from(ranked: RankedNode<'a, RANK>) -> Self {
        Self {
            rank: RANK,
            node: ranked.node(),
        }
    }
}

/// Insert refusal: the set already holds `CAP` members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RankedSetError {
    /// No free cell. The set keeps every member it already had — a refusal
    /// never evicts, because a member that vanished from the stop order is
    /// exactly the node a teardown loop forgets to stop.
    Full,
}

/// Fixed-capacity, rank-ordered set of nodes.
///
/// Insertion places each member at its rank position (highest first, equal
/// ranks in insertion order), so `iter()` is the stop order and a teardown
/// loop cannot accidentally read registration order. Reads are a walk over
/// `&entries[..len]`; insertion is one backward shift inside the array.
///
/// ```
/// use snubber::{RankedNode, RankedSet, SnubberNode};
///
/// let engine = SnubberNode::desktop("engine");
/// let log_sink = SnubberNode::desktop("log_sink");
///
/// let mut stop: RankedSet<'_, 4> = RankedSet::new();
/// // Registered sink-first, stopped engine-first: the rank decides.
/// assert!(stop.insert(RankedNode::<1>::new(&log_sink)).is_ok());
/// assert!(stop.insert(RankedNode::<9>::new(&engine)).is_ok());
///
/// let order: Vec<String> = stop.iter().map(|e| e.name().as_str().to_owned()).collect();
/// assert_eq!(order, vec!["engine".to_string(), "log_sink".to_string()]);
/// ```
#[derive(Debug)]
pub struct RankedSet<'a, const CAP: usize> {
    entries: [Option<RankedEntry<'a>>; CAP],
    len: usize,
}

impl<'a, const CAP: usize> Default for RankedSet<'a, CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, const CAP: usize> RankedSet<'a, CAP> {
    /// Empty set (`CAP` members at most).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [None; CAP],
            len: 0,
        }
    }

    /// Admit one member at its type's rank.
    ///
    /// A full set refuses with [`RankedSetError::Full`] and leaves its
    /// order untouched; it never drops an existing member to make room.
    pub fn insert<E: Into<RankedEntry<'a>>>(&mut self, member: E) -> Result<(), RankedSetError> {
        if self.len == CAP {
            return Err(RankedSetError::Full);
        }
        let member = member.into();
        // Highest rank first; equal ranks keep insertion order, because a
        // tie means "these are independent" and reversing them per insert
        // would make the order depend on how many members came before.
        let mut at = self.len;
        while at > 0 {
            let Some(prev) = self.entries[at - 1] else {
                break;
            };
            if prev.rank() >= member.rank() {
                break;
            }
            self.entries[at] = Some(prev);
            at -= 1;
        }
        self.entries[at] = Some(member);
        self.len += 1;
        Ok(())
    }

    /// Members in stop order (highest rank first).
    pub fn iter(&self) -> impl Iterator<Item = &RankedEntry<'a>> {
        self.entries[..self.len].iter().flatten()
    }

    /// Member at stop position `index` (`0` = stopped first).
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&RankedEntry<'a>> {
        if index < self.len {
            self.entries[index].as_ref()
        } else {
            None
        }
    }

    /// Members currently admitted.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` when the set holds no member.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Compile-time capacity (`CAP`).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        CAP
    }

    /// Forget every member (nodes keep living; only the set clears).
    pub fn clear(&mut self) {
        self.entries.fill(None);
        self.len = 0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_ranked_set_orders_by_rank_not_by_registration() {
        let slow = SnubberNode::desktop("slow");
        let middle = SnubberNode::desktop("middle");
        let first_out = SnubberNode::desktop("first_out");

        let mut set: RankedSet<'_, 4> = RankedSet::new();
        set.insert(RankedNode::<1>::new(&slow)).unwrap();
        set.insert(RankedNode::<9>::new(&first_out)).unwrap();
        set.insert(RankedNode::<4>::new(&middle)).unwrap();

        let order: Vec<u8> = set.iter().map(RankedEntry::rank).collect();
        assert_eq!(
            order,
            vec![9, 4, 1],
            "a teardown loop reads the array front to back"
        );
        assert_eq!(set.get(0).map(RankedEntry::rank), Some(9));
        assert_eq!(
            set.get(0).map(|e| e.name().as_str().to_string()),
            Some("first_out".to_string())
        );
        assert_eq!(set.len(), 3);
        assert_eq!(set.capacity(), 4);
        assert!(!set.is_empty());
    }

    #[test]
    fn equal_ranks_keep_registration_order() {
        let a = SnubberNode::desktop("a");
        let b = SnubberNode::desktop("b");
        let c = SnubberNode::desktop("c");

        let mut set: RankedSet<'_, 3> = RankedSet::new();
        set.insert(RankedNode::<5>::new(&a)).unwrap();
        set.insert(RankedNode::<5>::new(&b)).unwrap();
        set.insert(RankedNode::<5>::new(&c)).unwrap();

        let names: Vec<String> = set.iter().map(|e| e.name().as_str().to_string()).collect();
        assert_eq!(names, vec!["a", "b", "c"], "a tie is not a reordering");
    }

    #[test]
    fn a_full_set_refuses_and_keeps_its_order() {
        let a = SnubberNode::desktop("a");
        let b = SnubberNode::desktop("b");
        let c = SnubberNode::desktop("c");

        let mut set: RankedSet<'_, 2> = RankedSet::new();
        set.insert(RankedNode::<1>::new(&a)).unwrap();
        set.insert(RankedNode::<2>::new(&b)).unwrap();
        assert_eq!(
            set.insert(RankedNode::<7>::new(&c)),
            Err(RankedSetError::Full),
            "the highest-ranked member is refused rather than evicting a stop step"
        );
        assert_eq!(set.len(), 2);
        let order: Vec<u8> = set.iter().map(RankedEntry::rank).collect();
        assert_eq!(order, vec![2, 1], "a refusal changes nothing");
        assert!(set.get(2).is_none());
    }

    #[test]
    fn the_rank_is_the_type_not_a_stored_argument() {
        let node = SnubberNode::desktop("typed");
        assert_eq!(RankedNode::<7>::new(&node).rank(), 7);
        assert_eq!(RankedNode::<0>::new(&node).rank(), 0);

        let entry: RankedEntry<'_> = RankedNode::<3>::new(&node).into();
        assert_eq!(entry.rank(), 3);
        assert_eq!(entry.node().name().as_str(), "typed");

        let mut set: RankedSet<'_, 2> = RankedSet::new();
        set.insert(RankedNode::<3>::new(&node)).unwrap();
        set.clear();
        assert!(set.is_empty());
        assert!(set.get(0).is_none());
    }
}
