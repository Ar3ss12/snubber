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

/// Open-call audit WITH ORDER: split node.rs into function chunks; a
/// chunk that wakes must release the node lock (`drop(inner)`) BEFORE
/// its first wake site. Replaces the old count-based assert
/// (`drops >= wakes.min(drops + 1)`), which was true by construction
/// and proved nothing.
#[test]
fn wakes_happen_after_drop_in_node() {
    let node = src("src/node.rs");
    let wake_verbs = ["notify_all", "w.wake()"];
    let lines: Vec<&str> = node.lines().collect();

    // Function boundaries: any line that OPENS a function item. Comments
    // never start with `fn`, so a trimmed `fn`/`pub fn` prefix is a
    // boundary whether the item sits at file level or inside an impl.
    let mut starts: Vec<usize> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if t.starts_with("fn ") || t.starts_with("pub fn ") || t.starts_with("pub(crate) fn ") {
            starts.push(idx);
        }
    }
    starts.push(lines.len());

    let mut woke_chunks = 0usize;
    for window in starts.windows(2) {
        let (from, to) = (window[0], window[1]);
        let first_wake = (from..to).find(|&i| {
            let code = lines[i].trim_start();
            !code.starts_with("//") && wake_verbs.iter().any(|v| lines[i].contains(v))
        });
        let Some(wake_at) = first_wake else {
            continue;
        };
        woke_chunks += 1;
        assert!(
            (from..wake_at).any(|i| lines[i].contains("drop(inner)")),
            "open-call audit: function at node.rs:{} wakes ({}) while an earlier \
             `drop(inner)` is absent in the same function — the node lock must be \
             released before every wake",
            wake_at + 1,
            lines[wake_at].trim()
        );
    }
    assert!(
        woke_chunks > 0,
        "audit ran on an empty function split — boundary heuristic rotted"
    );
}

/// Rollback-call-site audit: Invariant 6 extended to the user's own closure.
///
/// The open-call law is what makes rollback re-entrancy return instead of
/// deadlocking (measured in `tests/rollback_guardrails.rs`), so the shape
/// worth gating is: a rollback closure is NEVER invoked while the node
/// ledger guard is still live in the same function. If someone moves a
/// `settle`/`record` call under the lock and leaves the rollback below it,
/// the user's code runs inside `Mutex<NodeInner>` — a self-deadlock on a
/// re-entrant admission, plus a lock held for the whole (foreign, possibly
/// wedged) body. No runtime test can enumerate that; a static scan can.
///
/// Site inventory is pinned on purpose: 13 invocation sites across three
/// files (plan time guessed 8 — it missed the no-`std` branches and the
/// `drop(g)` hardware-rollback sites). Rename or add a call site and the
/// count assert fires, so the audit cannot silently shrink.
const AUDITED_FILES: &[&str] = &["src/lib.rs", "src/erased.rs", "src/node.rs"];
const EXPECTED_ROLLBACK_SITES: usize = 13;

/// Is this trimmed line an invocation of a caller-supplied rollback body?
fn is_rollback_invocation(trimmed: &str) -> bool {
    trimmed == "rollback();"
        || trimmed == "slots.rollback();"
        || trimmed.contains("AssertUnwindSafe(rollback)")
        || trimmed.starts_with("drop(g); // hardware rollback")
        || trimmed.starts_with("drop(g); // `|| {}`")
}

/// Function spans as `(from, to)` line indices, using the same boundary
/// heuristic as `wakes_happen_after_drop_in_node`.
fn fn_spans(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut starts: Vec<usize> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if t.starts_with("fn ")
            || t.starts_with("pub fn ")
            || t.starts_with("pub(crate) fn ")
            || t.starts_with("async fn ")
            || t.starts_with("pub async fn ")
        {
            starts.push(idx);
        }
    }
    starts.push(lines.len());
    starts.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Offenders: rollback invocations reached while an earlier lock-take in the
/// same function has not been released by a `drop(` line yet.
///
/// Conservative and line-ordered (not brace-scoped), matching the style of
/// the other gates here: `lock_ledger(`/`.lock()`/`.read()`/`.write()` open a
/// window, any `drop(` closes it. Comment lines are skipped so doc prose
/// about locks cannot trip the scan.
fn rollback_sites_under_a_lock(source: &str) -> Vec<String> {
    let mut offenders = Vec::new();
    let mut site_count = 0usize;
    let lines: Vec<&str> = source.lines().collect();
    let span_of = |idx: usize| -> (usize, usize) {
        fn_spans(&lines)
            .into_iter()
            .find(|(from, to)| *from <= idx && idx < *to)
            .unwrap_or((0, lines.len()))
    };
    for (idx, line) in lines.iter().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") || !is_rollback_invocation(code) {
            continue;
        }
        site_count += 1;
        let (from, _to) = span_of(idx);
        let mut held = false;
        for prev in lines[from..idx].iter() {
            let p = prev.trim_start();
            if p.starts_with("//") {
                continue;
            }
            if p.contains("drop(") {
                held = false;
            }
            if p.contains("lock_ledger(")
                || p.contains(".lock()")
                || p.contains(".read()")
                || p.contains(".write()")
            {
                held = true;
            }
        }
        if held {
            offenders.push(format!("{}:{}: {}", "src", idx + 1, code.trim()));
        }
    }
    let _ = site_count;
    offenders
}

#[test]
fn rollback_bodies_never_run_under_the_node_lock() {
    let mut total_sites = 0usize;
    for path in AUDITED_FILES {
        let source = src(path);
        let lines: Vec<&str> = source.lines().collect();
        total_sites += lines
            .iter()
            .filter(|l| {
                let c = l.trim_start();
                !c.starts_with("//") && is_rollback_invocation(c)
            })
            .count();
        let offenders = rollback_sites_under_a_lock(&source);
        assert!(
            offenders.is_empty(),
            "TRIPWIRE FIRED: a rollback closure is invoked while a lock taken \
             earlier in the same function is still held (Invariant 6 open-call \
             violated on the rollback path — user code would run inside the node \
             ledger lock). Offenders: {offenders:?}"
        );
    }
    assert_eq!(
        total_sites, EXPECTED_ROLLBACK_SITES,
        "rollback-site audit drifted: expected {EXPECTED_ROLLBACK_SITES} invocation \
         sites in {AUDITED_FILES:?}, found {total_sites}. Add/remove a call site? \
         Update the inventory in this test AND re-verify the shape it guards — \
         silently shrinking the audit is how this gate becomes theatre."
    );
}

/// Self-check: the scanner must flag a synthetic held-lock-before-rollback
/// snippet, otherwise the audit above reports green because it is blind.
#[test]
fn rollback_gate_detects_a_synthetic_held_lock() {
    let bad = "fn settle(&mut self) {\n\
               let inner = lock_ledger(&self.node);\n\
               rollback();\n\
               drop(inner);\n\
               }\n";
    let good = "fn settle(&mut self) {\n\
                let inner = lock_ledger(&self.node);\n\
                drop(inner);\n\
                rollback();\n\
                }\n";
    let offenders = rollback_sites_under_a_lock(bad);
    assert_eq!(
        offenders.len(),
        1,
        "the rollback scanner is blind: a synthetic `lock_ledger` + `rollback()` \
         with no release produced no offender"
    );
    assert!(
        rollback_sites_under_a_lock(good).is_empty(),
        "the rollback scanner flags a released lock — the audit would be all noise"
    );
}
