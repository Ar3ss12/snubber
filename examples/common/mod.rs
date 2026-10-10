//! Shared torture-lab helper: deterministic LCG, no `rand` crate.
//!
//! Included into each `examples/torture_*` binary via `#[path]`.
//! Kept dependency-free on purpose: the chaos lab must not drag in
//! anything the `snubber` core itself would refuse.

/// Deterministic LCG (Numerical Recipes constants).
pub struct Lcg(pub u64);

impl Lcg {
    /// Next pseudo-random value (upper bits — the better-mixed half).
    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}
