//! Capture-capable CPU sampler on `TrackedGuard`.
//!
//! The plain `fn() -> u64` sampler cannot see per-thread state (a thread
//! handle, a worker's own counter). These tests pin the shared-closure
//! path: the guard reads the captured counter, classifies SPIN vs SLEEP
//! from it, and stays within its documented size ceiling. The last test
//! guards the layer boundary — `node` is `std`-gated, so a stray
//! unguarded `pub use node` in `src/lib.rs` would pull the alloc-typed
//! field into the `core` build.

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use snubber::{SnubberNode, StallKind, TimeoutCheck, TrackedGuard};

#[test]
fn sampler_shared_sees_the_closure_state() {
    let cpu_ms = Arc::new(AtomicU64::new(0));
    let node = SnubberNode::builder("sampler/shared-spin")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .build();
    let sampler = {
        let cpu_ms = Arc::clone(&cpu_ms);
        move || cpu_ms.fetch_add(100, Ordering::SeqCst)
    };
    let guard = node
        .enter(1, 1, || {})
        .expect("admit")
        .with_cpu_sampler_shared(Arc::new(sampler));
    std::thread::sleep(core::time::Duration::from_millis(10));
    assert_eq!(
        guard.stall_kind(),
        StallKind::CpuSpin,
        "captured counter advancing past the CPU budget must classify as SPIN"
    );
    match guard.check_timeout() {
        Err(TimeoutCheck::SpinDetected) => {}
        _ => panic!("shared hot spin must settle as SpinDetected"),
    }
    assert_eq!(node.stats().timeouts_fired, 1);
}

#[test]
fn sampler_shared_zero_delta_is_sleep_blocked() {
    let cpu_ms = Arc::new(AtomicU64::new(0));
    let node = SnubberNode::builder("sampler/shared-sleep")
        .max_in_flight(1)
        .timeout_ms(1)
        .cpu_budget_ms(50)
        .build();
    let sampler = {
        let cpu_ms = Arc::clone(&cpu_ms);
        move || cpu_ms.load(Ordering::SeqCst)
    };
    let guard = node
        .enter(1, 1, || {})
        .expect("admit")
        .with_cpu_sampler_shared(Arc::new(sampler));
    std::thread::sleep(core::time::Duration::from_millis(10));
    assert_eq!(
        guard.stall_kind(),
        StallKind::SleepBlocked,
        "captured counter standing still while wall blew is SLEEP, not SPIN"
    );
    match guard.check_timeout() {
        Err(TimeoutCheck::TimedOut) => {}
        _ => panic!("shared still counter must settle as TimedOut"),
    }
    assert_eq!(node.stats().timeouts_fired, 1);
}

#[test]
fn track_guard_stays_within_the_size_ceiling() {
    let size = size_of::<TrackedGuard<fn()>>();
    println!("TrackedGuard<fn()> = {size} bytes");
    assert!(
        size <= 320,
        "TrackedGuard grew past the 320 B ceiling: {size} bytes"
    );
}

/// The alloc-typed sampler lives in `node`, which is `#[cfg(feature = "std")]`.
/// An unguarded root re-export would compile the field into the `core`
/// build and break the bare-metal lane — pin the gate in text.
#[test]
fn lib_root_reexports_node_only_under_std() {
    let lib_rs = include_str!("../src/lib.rs");
    assert!(
        lib_rs.contains("pub mod node"),
        "tripwire target missing: `pub mod node` no longer in src/lib.rs"
    );
    let lines: Vec<&str> = lib_rs.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !(trimmed.starts_with("pub use node") || trimmed.starts_with("pub mod node")) {
            continue;
        }
        let guarded = lines[..idx]
            .iter()
            .rev()
            .take(4)
            .any(|above| above.trim() == "#[cfg(feature = \"std\")]");
        assert!(
            guarded,
            "src/lib.rs:{} exports `node` without #[cfg(feature = \"std\")]: {trimmed}",
            idx + 1
        );
    }
}
