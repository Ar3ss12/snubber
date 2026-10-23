//! Node identity: a compile-time label or an `alloc`-shared dynamic one.
//!
//! [`NodeName::Static`] is the zero-allocation default that every existing
//! call site produces from a `&'static str`. [`NodeName::Owned`] carries an
//! `Arc<str>` for nodes named at runtime (per-tenant shards, PID-suffixed
//! workers, test fixtures). Cloning is one atomic increment on the owned
//! arm and a plain fat-pointer copy on the static arm.
//!
//! Hot telemetry frames (`SnapshotV1`, `FlightSlot`, `Anomaly`, wire
//! headers) stay `Copy` and `&'static str`; only the cold identity carried
//! by [`FaultRecord`](crate::FaultRecord) and
//! [`SnubberNode::name`](crate::SnubberNode::name) is dynamic.

use core::fmt;
use core::hash::{Hash, Hasher};

#[cfg(target_has_atomic = "ptr")]
use alloc::sync::Arc;

/// Stable node identity: static literal or shared owned string.
#[derive(Clone)]
pub enum NodeName {
    /// Compile-time literal — no allocation, `Copy`-like clone.
    Static(&'static str),
    /// Runtime name shared through `Arc<str>`; available where the target
    /// has pointer-width atomics.
    #[cfg(target_has_atomic = "ptr")]
    Owned(Arc<str>),
}

impl NodeName {
    /// Borrows the label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Static(s) => s,
            #[cfg(target_has_atomic = "ptr")]
            Self::Owned(s) => s,
        }
    }

    /// `true` if the label is a `&'static str` (no allocation).
    #[must_use]
    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static(_))
    }

    /// Static label when this name was built from a literal.
    ///
    /// `Copy` telemetry frames (metrics, anomaly verdicts) are `core`-only
    /// and keep `&'static str`; use this accessor to export into them and
    /// pick an explicit fallback for owned names.
    #[must_use]
    pub fn as_static(&self) -> Option<&'static str> {
        match self {
            Self::Static(s) => Some(s),
            #[cfg(target_has_atomic = "ptr")]
            Self::Owned(_) => None,
        }
    }

    /// Wraps a runtime string in the shared owned arm.
    #[cfg(target_has_atomic = "ptr")]
    #[must_use]
    pub fn owned(name: impl Into<Arc<str>>) -> Self {
        Self::Owned(name.into())
    }
}

impl Default for NodeName {
    #[inline]
    fn default() -> Self {
        Self::Static("")
    }
}

impl From<&'static str> for NodeName {
    #[inline]
    fn from(name: &'static str) -> Self {
        Self::Static(name)
    }
}

#[cfg(target_has_atomic = "ptr")]
impl From<alloc::string::String> for NodeName {
    #[inline]
    fn from(name: alloc::string::String) -> Self {
        Self::Owned(Arc::from(name))
    }
}

#[cfg(target_has_atomic = "ptr")]
impl From<Arc<str>> for NodeName {
    #[inline]
    fn from(name: Arc<str>) -> Self {
        Self::Owned(name)
    }
}

impl fmt::Display for NodeName {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for NodeName {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same shape as `&str`'s Debug: `"super/worker"`.
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl PartialEq for NodeName {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for NodeName {}

impl Hash for NodeName {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq<str> for NodeName {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for NodeName {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<NodeName> for str {
    #[inline]
    fn eq(&self, other: &NodeName) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<NodeName> for &str {
    #[inline]
    fn eq(&self, other: &NodeName) -> bool {
        *self == other.as_str()
    }
}
