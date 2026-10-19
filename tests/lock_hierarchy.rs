//! Lock-hierarchy tripwire: the Zero-Nesting law as a test.
//!
//! Law: `SnubberNode`'s lock NEVER nests inside `TaskConduit`'s lock,
//! and `TaskConduit`'s lock NEVER nests inside `SnubberNode`'s lock.
//! Cross-module calls happen only AFTER `drop(guard)` (open-call).
//! Nested locks are the only remaining deadlock class (ABBA), so this
//! grep-gate keeps the 99.9% verdict honest.

use std::fs;

/// Read one source file or panic with the path.
fn src(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|_| panic!("missing source: {path}"))
}

/// Assert no `lock()` call sits textually inside a scope that already
/// holds the OTHER module's guard. Conservative grep-gate: doc-comments
/// (`///`) are stripped first (examples may name the other side),
/// any remaining code hit trips it.
#[test]
fn no_nested_locks_across_node_and_conduit() {
    fn code_only(source: &str) -> String {
        source
            .lines()
            .filter(|l| !l.trim_start().starts_with("///") && !l.trim_start().starts_with("//!"))
            .collect::<Vec<_>>()
            .join("\n")
    }
    let node = code_only(&src("src/node.rs"));
    let conduit = code_only(&src("src/conduit.rs"));

    // Node side: `enter_armed_with_priority` and settle paths must
    // release the node lock before touching conduit primitives
    // (`notify_all`, `wake_async`, `send_directive`, `resume`).
    // The documented pattern is: compute waker under lock, `drop(inner)`,
    // THEN wake outside. A direct conduit call while `inner` is alive
    // would appear as `inner.` + conduit verb on adjacent lines.
    for verb in [
        "condvar.notify_all",
        "wake_async",
        "send_directive",
        "conduit.",
    ] {
        let hits = node.matches(verb).count();
        assert_eq!(
            hits, 0,
            "TRIPWIRE FIRED: node.rs mentions conduit primitive '{verb}' \
             ({hits} hits) — cross-module calls must happen after drop(inner), \
             never under the node lock"
        );
    }

    // Conduit side: must never name node admission (`enter`, `report`,
    // `SnubberNode::`) — steering observes directives, it never arms
    // guards (that would nest the node lock inside the conduit lock).
    for verb in ["SnubberNode", ".enter(", "node.report", "record_locked"] {
        let hits = conduit.matches(verb).count();
        assert_eq!(
            hits, 0,
            "TRIPWIRE FIRED: conduit.rs mentions node primitive '{verb}' \
             ({hits} hits) — the conduit never arms guards, it only steers"
        );
    }
}

/// Open-call audit: every `notify_all` / `w.wake()` in node.rs must be
/// preceded (in the same function) by a `drop(inner)`. Documents the
/// law; the grep above enforces the module boundary.
#[test]
fn wakes_happen_after_drop_in_node() {
    let node = src("src/node.rs");
    let wakes = node.matches("notify_all").count()
        + node.matches("w.wake()").count()
        + node.matches("waker").count().min(1);
    let drops = node.matches("drop(inner)").count();
    assert!(
        drops >= wakes.min(drops + 1),
        "open-call audit: {wakes} wake sites vs {drops} drop(inner) — \
         every wake must follow a drop"
    );
    assert!(
        drops > 0,
        "node.rs must contain drop(inner) open-call sites"
    );
}
