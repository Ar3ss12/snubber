//! Liveness witness contract (`core::sentinel`).
//!
//! The crate names the verdict and never kills: these tests pin the three
//! rules an application relies on — death outranks silence, `None` is
//! "cannot observe" rather than "dead", and the grace comparison is strict.

use std::sync::Arc;

use snubber::{
    classify_liveness, NoWitness, ProcessWitness, Sentinel, SentinelConfig, SentinelSample,
    SentinelVerdict,
};

fn sample(idle_ms: u64, armed: bool, witness_alive: Option<bool>) -> SentinelSample {
    SentinelSample {
        idle_ms,
        armed,
        witness_alive,
    }
}

struct FixedWitness(Option<bool>);

impl ProcessWitness for FixedWitness {
    fn alive(&self) -> Option<bool> {
        self.0
    }

    fn backend_name(&self) -> &'static str {
        "fixed"
    }
}

#[test]
fn witness_is_object_safe_and_names_its_backend() {
    let witness: Arc<dyn ProcessWitness> = Arc::new(FixedWitness(Some(true)));
    assert_eq!(witness.alive(), Some(true));
    assert_eq!(witness.backend_name(), "fixed");
    assert_eq!(NoWitness.alive(), None);
    assert_eq!(NoWitness.backend_name(), "none (cannot observe)");
}

#[test]
fn dead_verdict_carries_the_armed_flag() {
    let cfg = SentinelConfig::new(1_000);
    assert_eq!(
        classify_liveness(cfg, sample(0, true, Some(false))),
        SentinelVerdict::Dead { armed: true }
    );
    assert_eq!(
        classify_liveness(cfg, sample(0, false, Some(false))),
        SentinelVerdict::Dead { armed: false }
    );
}

#[test]
fn cannot_observe_never_mints_dead() {
    let cfg = SentinelConfig::new(100);
    assert_eq!(
        classify_liveness(cfg, sample(0, true, None)),
        SentinelVerdict::Alive
    );
    assert_eq!(
        classify_liveness(cfg, sample(500, true, None)),
        SentinelVerdict::Silent { idle_ms: 500 }
    );
}

#[test]
fn grace_boundary_is_strict() {
    let cfg = SentinelConfig::new(250);
    assert_eq!(
        classify_liveness(cfg, sample(250, true, Some(true))),
        SentinelVerdict::Alive,
        "idle == grace is still Alive"
    );
    assert_eq!(
        classify_liveness(cfg, sample(251, true, Some(true))),
        SentinelVerdict::Silent { idle_ms: 251 }
    );
}

#[test]
fn zero_grace_disables_silent_only() {
    let cfg = SentinelConfig::new(0);
    assert_eq!(cfg.idle_grace_ms(), 0);
    assert_eq!(
        classify_liveness(cfg, sample(u64::MAX, true, Some(true))),
        SentinelVerdict::Alive
    );
    assert_eq!(
        classify_liveness(cfg, sample(0, true, Some(false))),
        SentinelVerdict::Dead { armed: true }
    );
}

#[test]
fn sentinel_tracks_the_last_verdict() {
    let mut sentinel = Sentinel::new(SentinelConfig::new(100));
    assert_eq!(sentinel.last(), SentinelVerdict::Alive);
    assert_eq!(
        sentinel.observe(sample(500, true, Some(true))),
        SentinelVerdict::Silent { idle_ms: 500 }
    );
    assert_eq!(sentinel.last(), SentinelVerdict::Silent { idle_ms: 500 });
    assert_eq!(
        sentinel.observe(sample(1, false, Some(false))),
        SentinelVerdict::Dead { armed: false }
    );
    assert_eq!(sentinel.last(), SentinelVerdict::Dead { armed: false });
}
