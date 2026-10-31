//! Doc tripwires: the FLOOR MAP anchors in `src/node.rs` must never rot,
//! the L1–L5 layer map must keep its headings, order and canonical
//! one-line tagline across the three public docs, and every module path
//! quoted in prose must exist on disk. README snippets are compiled by
//! `cargo test --doc` through a `#[cfg(doctest)]` hook in `src/lib.rs`, and
//! this file guards that hook, each fence's compile verdict and the absolute
//! asset hrefs the registry page needs. Measured latencies have exactly one
//! home — BENCHMARKS.md, pinned to the committed bench artifact; README and
//! the User Guide quote no numbers.
//!
//! Line numbers in comments die the day someone adds an import. Text tags
//! don't. If a refactor deletes or renames an anchor, this test goes red
//! and names the missing tag.
//!
//! Which of these checks are semantic oracles and which are drift
//! tripwires: docs/INVARIANTS.md, section 2, "Gate taxonomy".
//!
//! Twin audit note (compile-only doc-tests): the 6 `rust,no_run` fences
//! (`spawn_guarded`, `console_node`, `ErasedGuard`, `assert_no_anomalies`,
//! `wait_while_paused`, `LegalMove`) are guarded by REAL twins in the
//! integration suite: `supervisor.rs::spawn_guarded_*`,
//! `circuit_breaker.rs` + `stdin_digest.rs` (`console_node`),
//! `erased.rs` + `worker_lifecycle.rs` (`ErasedGuard`),
//! `diagnostics.rs` renderers, `conduit.rs` pause/resume,
//! `scoped_transactions.rs` (`LegalMove`). A change to those APIs must keep
//! both sides compiling — cargo does that per doc-fence; the twins prove the
//! behavior itself. `test_no_run_fences_match_header` keeps this list
//! honest.

/// Every anchor named in the FLOOR MAP header must exist exactly once in
/// the node source (header mention + one definition site = 2 hits).
#[test]
fn test_node_floor_map_anchors_exist() {
    let source = std::fs::read_to_string("src/node.rs")
        .expect("Failed to read src/node.rs for floor map audit");

    let required_anchors = [
        "// [ANCHOR: RECORDS]",
        "// [ANCHOR: BUILDER]",
        "// [ANCHOR: CANONICAL_ENTRY]",
        "// [ANCHOR: LOW_LEVEL_ENTRY]",
        "// [ANCHOR: ADMISSION_PHASE]",
        "// [ANCHOR: BREAKER_STATS]",
    ];

    for anchor in required_anchors {
        let hits = source.matches(anchor).count();
        assert!(
            hits == 2,
            "TRIPWIRE FIRED: anchor '{anchor}' found {hits} times in src/node.rs \
             (want exactly 2: header index + definition site). \
             Update the FLOOR MAP.",
        );
    }
}

/// Every low-level entry must carry the Canonical Indirection Warning pointing at the
/// canonical worker entry. A newcomer hovering over `enter` in an IDE must
/// see "prefer probe_erased_auto" before invoking low-level primitives.
#[test]
fn test_low_level_entries_point_at_canonical() {
    let source =
        std::fs::read_to_string("src/node.rs").expect("Failed to read src/node.rs for entry audit");

    const LOW_LEVEL_METHODS: &[&str] = &[
        "pub fn enter<F: FnOnce()>(",
        "pub fn enter_with_priority<F: FnOnce()>(",
        "pub fn enter_with_generation<F: FnOnce()>(",
        "pub fn probe_erased<F: FnOnce() + Send + 'static>(",
        "pub fn enter_auto<F: FnOnce()>(",
        "pub fn enter_checked_auto<P, V, F: FnOnce()>(",
        "pub fn enter_checked<P, V, F: FnOnce()>(",
        "pub fn enter_supervised_erased<F>(",
    ];

    let warning = "prefer the canonical [`Self::probe_erased_auto`]";

    for method in LOW_LEVEL_METHODS {
        let pos = source
            .find(method)
            .unwrap_or_else(|| panic!("Method {method} not found in src/node.rs"));
        let doc_window = &source[pos.saturating_sub(2500)..pos];
        assert!(
            doc_window.contains(warning),
            "Method {method} is missing the Canonical Indirection Warning banner"
        );
    }

    // `probe_erased_auto` is the canonical target and must NOT carry the redirection itself.
    let canonical_pos = source
        .find("pub fn probe_erased_auto<")
        .expect("Canonical entry missing");
    let canonical_doc = &source[canonical_pos.saturating_sub(600)..canonical_pos];

    assert!(
        !canonical_doc.contains(warning),
        "Canonical entry must not carry a redirection warning to itself"
    );

    // The canonical entry advertises itself with the canonical header.
    assert!(
        source.contains("# 🟢 CANONICAL APPLICATION ENTRY"),
        "TRIPWIRE FIRED: canonical header missing from probe_erased_auto.",
    );
}

/// The erased-guard bulkhead boundary must stay documented: the erased PROBE
/// path releases its slot at admission, so `max_in_flight` bounds admissions,
/// not live erased transactions — a ceiling over live transactions belongs to
/// `enter_erased_holding_auto`.
///
/// Home of the statement: `docs/USER_GUIDE.md` (Layer 2, "Scope correction")
/// and the `probe_erased_auto` doc comment — NOT the README. On 2026-10-27 the
/// owner had the six-line caveat deleted from the README's guarantee list,
/// where it read as a legal disclaimer at the end of the wins; the pin moved
/// with the text instead of being dropped, so the correction is still guarded.
#[test]
fn test_erased_probe_bulkhead_scope_stays_documented() {
    let guide = std::fs::read_to_string("docs/USER_GUIDE.md")
        .expect("Failed to read docs/USER_GUIDE.md for bulkhead-scope audit");
    assert!(
        guide.contains("not a lease on the slot for the worker's lifetime"),
        "TRIPWIRE FIRED: the User Guide lost the erased-path 'Scope correction' \
         — `max_in_flight` on the erased probe path bounds admissions, and a \
         reader must still be told that."
    );
    assert!(
        guide.contains("serialize two live erased workers"),
        "TRIPWIRE FIRED: the User Guide's bulkhead-scope correction lost its \
         boundary statement."
    );
    assert!(
        guide.contains("enter_erased_holding_auto"),
        "TRIPWIRE FIRED: the User Guide stopped naming the HOLDING entry \
         (`enter_erased_holding_auto`) as the ceiling over live transactions."
    );
    let source = std::fs::read_to_string("src/node.rs")
        .expect("Failed to read src/node.rs for bulkhead-scope audit");
    let pos = source
        .find("pub fn probe_erased_auto<")
        .expect("Canonical entry missing");
    let doc_window = &source[pos.saturating_sub(1200)..pos];
    assert!(
        doc_window.contains("Bulkhead scope"),
        "TRIPWIRE FIRED: probe_erased_auto doc lost the bulkhead-scope note."
    );
    let readme = std::fs::read_to_string("README.md")
        .expect("Failed to read README.md for bulkhead-scope audit");
    assert!(
        readme.contains("### Bulkhead Scope"),
        "TRIPWIRE FIRED: README lost the 'Bulkhead Scope' section that the \
         probe_erased_auto doc comment links to."
    );
}

/// The README carried a *Platform boundary — what the crate cannot see* page
/// that enumerated a secure desktop, a removed low-level hook, a button left
/// held and the counters nobody sampled. A page that lists what a library does
/// not do sells absence as a feature: the contract is the reaction the crate
/// builds (rollback body, its budget, the outside poller, the ranked teardown),
/// not the platform state it never touches. The page is gone and this tripwire
/// keeps the frame from returning in any spelling.
#[test]
fn test_readme_does_not_advertise_what_the_crate_cannot_see() {
    let readme = read_doc("README.md");
    assert!(
        readme.len() > 8_000 && readme.contains("## When to use / when not"),
        "TRIPWIRE FIRED: README.md is not the document this scan expects — a \
         truncated or replaced file cannot pass by having nothing to find"
    );

    let lower = readme.to_lowercase();
    for phrase in [
        "platform boundary",
        "what the crate cannot see",
        "what the crate can't see",
        "platform monitor",
        "blind spot",
    ] {
        assert!(
            !lower.contains(phrase),
            "TRIPWIRE FIRED: README.md carries `{phrase}` — the README does not \
             enumerate what the crate cannot do. State the guarantee the crate \
             provides; platform edges belong to the application and to its own \
             documentation"
        );
    }
    for token in [
        "OpenInputDesktop",
        "SetWindowsHookEx",
        "SendInput",
        "mouse_event",
    ] {
        assert!(
            !readme.contains(token),
            "TRIPWIRE FIRED: README.md names `{token}` — that API appears only in \
             a page about what this crate does not call. The README describes the \
             reaction the crate owns, not the OS calls it leaves to the caller"
        );
    }
}

/// Rollback re-entrancy must be documented as MEASURED behavior, not as the
/// old folklore.
///
/// The guide used to forbid calling back into the node from a rollback with
/// the reason "(that deadlocks)". Probes and `tests/rollback_guardrails.rs`
/// (2026-10-27) say otherwise: rollback bodies run outside the node lock
/// (Invariant 6, open-call), so re-entry returns — `BulkheadFull` when the
/// node is full because the rolling-back guard still holds its slot,
/// `Poll::Pending` on the async path, granted on the erased path. A law that
/// names the wrong failure sends responders hunting for a deadlock that
/// cannot happen while the real wedge (a re-taken FOREIGN mutex) stays
/// invisible. This oracle pins the corrected wording and bans the myth.
#[test]
fn test_rollback_reentry_law_matches_the_code() {
    let guide = std::fs::read_to_string("docs/USER_GUIDE.md")
        .expect("Failed to read docs/USER_GUIDE.md for rollback re-entrancy audit");
    let at = guide
        .find("**Re-entrancy is allowed, not free:**")
        .expect("TRIPWIRE FIRED: docs/USER_GUIDE.md lost the rollback re-entrancy rule");
    let rule = guide[at..guide[at..]
        .find("\n4. ")
        .map(|n| at + n)
        .unwrap_or(guide.len())]
        .to_owned();
    for token in [
        "does **not** deadlock",
        "BulkheadFull",
        "holds its slot until it returns",
        "Poll::Pending",
        "re-locking a mutex",
        "probe_erased_auto",
    ] {
        assert!(
            rule.contains(token),
            "TRIPWIRE FIRED: the rollback re-entrancy rule lost `{token}` — the \
             measured behavior is: sync refuses at capacity, async parks, erased \
             is granted, and the slot is held for the whole body. Rule reads: {rule}"
        );
    }
    let deadlocks_myth = rule.to_lowercase().contains("that deadlocks")
        || rule.to_lowercase().contains("(deadlocks)");
    assert!(
        !deadlocks_myth,
        "TRIPWIRE FIRED: the rollback re-entrancy rule is back to claiming a \
         deadlock. Measured 2026-10-27 in tests/rollback_guardrails.rs: it does \
         not deadlock."
    );

    let invariants = std::fs::read_to_string("docs/INVARIANTS.md")
        .expect("Failed to read docs/INVARIANTS.md for rollback re-entrancy audit");
    let at = invariants
        .find("### Invariant 6:")
        .expect("Invariant 6 heading missing from docs/INVARIANTS.md");
    let window = &invariants[at..invariants.len().min(at + 1600)];
    assert!(
        window.contains("ROLLBACK bodies"),
        "TRIPWIRE FIRED: Invariant 6 no longer states that rollback call sites \
         run with the ledger released — that is the reason re-entry returns."
    );
    assert!(
        window.contains("FOREIGN mutex"),
        "TRIPWIRE FIRED: Invariant 6 lost the honest limit: a caller's re-taken \
         mutex still hangs, and no node counter sees it."
    );
}

/// The host-runtime laws are the part readers get wrong most often, and a
/// doc that drifts back to "snubber replaces Mutex" or "cancellation is a
/// free abort" would ship a false promise. The section must keep: the
/// cancelled future books `ImplicitDrop`, a blocking rollback has a budget
/// and an outside poller, Rayon's rollback runs after the join, and
/// `SafeLockExt` wraps the same lock instead of replacing it.
#[test]
fn test_host_runtime_laws_are_stated() {
    let guide = read_doc("docs/USER_GUIDE.md");
    let readme = read_doc("README.md");

    const HEADING: &str = "### 9. Host runtimes:";
    let start = guide
        .find(HEADING)
        .expect("TRIPWIRE FIRED: docs/USER_GUIDE.md lost the Host runtimes section");
    let after = start + HEADING.len();
    let end = guide[after..]
        .find("\n## ")
        .map(|n| after + n)
        .unwrap_or(guide.len());
    let section = &guide[start..end];

    for token in [
        "TrackedGuard",
        "FaultKind::ImplicitDrop",
        "checkpoint",
        "state_snapshot",
        "rollback_budget_ms",
        "snubber::watchdog::poll_rollback",
        "scoped",
        "has joined",
        "best-effort",
        "revalidate",
        "adds no dependency",
        "does not replace `Mutex`",
    ] {
        assert!(
            section.contains(token),
            "TRIPWIRE FIRED: the Host runtimes section no longer states `{token}` — \
             a cancelled await books ImplicitDrop, a blocking rollback is budgeted \
             and polled from outside, Rayon rolls back after the join, and \
             SafeLockExt keeps the same Mutex"
        );
    }

    for (name, doc) in [("README.md", &readme), ("docs/USER_GUIDE.md", &guide)] {
        assert!(
            doc.contains("does not replace `Mutex`"),
            "TRIPWIRE FIRED: {name} must keep the law that SafeLockExt is not a \
             lock implementation — it wraps the same std Mutex and counts recoveries"
        );
        assert!(
            !doc.contains("replaces `Mutex`"),
            "TRIPWIRE FIRED: {name} now claims SafeLockExt replaces Mutex — it does \
             not: same lock, counted poison recovery, data still needs revalidation"
        );
    }
}

/// The badge row is a set of working instruments, not decoration: exactly
/// five badges, each answering one landing-page question, each with a real
/// target — and no numeric test count. The `tests-N passed` pill was deleted
/// on 2026-10-27: a test count is a CI-log fact, it drifts the moment a test
/// is added, and it reads as self-praise beside the CI badge that already
/// proves the suite is green. `scripts/check_test_count.sh` guards the same
/// rule from the script side; this pins the row's shape.
#[test]
fn test_readme_badges_are_instruments() {
    let readme = read_doc("README.md");
    let badges: Vec<&str> = readme
        .lines()
        .filter(|line| line.starts_with("[!["))
        .collect();
    assert_eq!(
        badges.len(),
        5,
        "TRIPWIRE FIRED: the README badge row has {} badges, the row is five \
         instruments (GitHub, Crates.io, docs.rs, CI, license) — check the \
         links and the count",
        badges.len()
    );

    const TARGETS: [&str; 5] = [
        "](https://github.com/Ar3ss12/snubber)",
        "](https://crates.io/crates/snubber)",
        "](https://docs.rs/snubber)",
        "](https://github.com/Ar3ss12/snubber/actions/workflows/ci.yml)",
        "](#license)",
    ];
    for target in TARGETS {
        assert!(
            badges.iter().any(|badge| badge.contains(target)),
            "TRIPWIRE FIRED: no badge links to '{target}' — every badge must be \
             a teleport a reader can actually use from crates.io or docs.rs"
        );
    }

    for badge in &badges {
        assert!(
            !badge.contains("]()"),
            "TRIPWIRE FIRED: badge links nowhere: {badge}"
        );
        assert!(
            !badge.contains("tests-") && !badge.contains("%20passed"),
            "TRIPWIRE FIRED: the README publishes a test count again — the CI \
             badge already proves the suite is green: {badge}"
        );
    }
}

/// README.md is a doctest source: src/lib.rs folds it in behind
/// `#[cfg(doctest)]`, so `cargo test --doc` compiles its snippets against the
/// real API. This test guards the mechanism, not the prose — delete the hook
/// and every fence silently stops being checked.
#[test]
fn test_readme_is_wired_into_doctests() {
    let lib = read_doc("src/lib.rs");
    assert!(
        lib.contains("#[cfg(doctest)]") && lib.contains("include_str!(\"../README.md\")"),
        "TRIPWIRE FIRED: src/lib.rs lost the README doctest hook — README.md \
         snippets are no longer compiled by `cargo test --doc`"
    );
}

/// Every Rust fence in README.md carries a compile verdict: `no_run` proves it
/// compiles without executing, and `ignore` is allowed only for the hand-rolled
/// WITHOUT half — an illustrative pattern with no types behind it. A rust fence
/// with no modifier would run on every CI pass, so it fails here instead.
#[test]
fn test_readme_fences_have_a_compile_verdict() {
    let readme = read_doc("README.md");
    let lines: Vec<&str> = readme.lines().collect();
    let mut compiled = 0usize;
    let mut ignored_lines = Vec::new();
    let mut bare_lines = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let Some(info) = line.strip_prefix("```rust") else {
            continue;
        };
        if info.is_empty() {
            bare_lines.push(idx + 1);
        } else if info.contains("ignore") {
            ignored_lines.push(idx + 1);
        } else if info.contains("no_run") {
            compiled += 1;
        } else {
            bare_lines.push(idx + 1);
        }
    }

    assert!(
        bare_lines.is_empty(),
        "TRIPWIRE FIRED: README.md line(s) {bare_lines:?} open a Rust fence with \
         no compile verdict — mark it rust,no_run (or rust,ignore for prose \
         pseudo-code that must be registered below)"
    );
    assert_eq!(
        compiled, 3,
        "TRIPWIRE FIRED: expected 3 rust,no_run README fences (quick start, \
         WITH-delta, PipeDrain feed), found {compiled} — a snippet was added \
         or removed; update this pin and the README doctest inventory"
    );
    assert_eq!(
        ignored_lines.len(),
        1,
        "TRIPWIRE FIRED: {ignored_lines:?} rust,ignore README fences, expected \
         exactly 1 (the hand-rolled WITHOUT block). Every ignore is unproven \
         code; adding one is a decision, not a refactor artifact"
    );
    let body = lines[ignored_lines[0]];
    assert!(
        body.contains("WITHOUT snubber"),
        "TRIPWIRE FIRED: the ignored README fence is no longer the WITHOUT-half; \
         it must compile instead of being excused: {body}"
    );
}

/// `cargo package` excludes `assets/`, and crates.io / docs.rs render this file
/// with no repository tree behind it, so a relative image path is a broken
/// picture there. Absolute HTTPS href, and the local file must still exist for
/// the GitHub view.
#[test]
fn test_readme_assets_are_absolute() {
    for (idx, line) in read_doc("README.md").lines().enumerate() {
        let Some(rest) = line.split("src=\"").nth(1) else {
            continue;
        };
        let href = rest.split('"').next().unwrap_or("");
        assert!(
            href.starts_with("https://"),
            "TRIPWIRE FIRED: README.md line {} embeds '{href}' — the registry \
             page cannot resolve a relative asset; use the \
             raw.githubusercontent.com URL",
            idx + 1,
        );
        if let Some(path) =
            href.strip_prefix("https://raw.githubusercontent.com/Ar3ss12/snubber/main/")
        {
            assert!(
                std::path::Path::new(path).exists(),
                "TRIPWIRE FIRED: README.md line {} points at '{path}', which is \
                 not in the tree at the branch the URL names",
                idx + 1,
            );
        }
    }
}

/// The one true layer map. Every public document names each layer with the
/// same bytes; only the surrounding heading syntax differs per file.
const CANONICAL_LAYERS: [&str; 5] = [
    "Core RAII Guard",
    "Concurrency Bulkhead",
    "Runtime-Agnostic Async",
    "Process Supervisor",
    "Diagnostics & Health",
];

/// The one sentence that defines each layer. It must appear within the
/// layer's entry (heading, or two lines below it) in all three docs, so a
/// doc cannot silently describe a different contract. Bodies are free to
/// differ — this is the semantic anchor, not a byte lock.
const CANONICAL_TAGLINES: [&str; 5] = [
    "Rollback is structural: an admitted guard reverts unless it is committed.",
    "Admission is capped; the breaker fails fast instead of queueing.",
    "One parked waker per wait; any executor drives it, with no runtime dependency.",
    "The supervisor owns the child process; the drain classifies its wire frames.",
    "One 512-byte snapshot answers whether the system is healthy.",
];

fn read_doc(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {path}: {err}"))
}

/// Returns `(line index, line)` for the single line starting with `prefix`.
/// Zero hits or duplicates both name the doc and the expected heading.
fn find_layer_line<'a>(text: &'a str, prefix: &str, doc: &str) -> (usize, &'a str) {
    let mut hits = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.starts_with(prefix));
    let (index, line) = hits
        .next()
        .unwrap_or_else(|| panic!("{doc}: missing layer heading '{prefix}'"));
    let duplicates = hits.count();
    assert!(
        duplicates == 0,
        "TRIPWIRE FIRED: {doc}: layer heading '{prefix}' appears {} times",
        duplicates + 1
    );
    (index, line)
}

/// L1–L5 must keep their canonical name, order AND tagline in `src/lib.rs`,
/// `README.md` and `docs/USER_GUIDE.md`. Rename a layer or rewrite its
/// one-line contract in one file and this goes red on `cargo test`, not in a
/// reader's head months later.
#[test]
fn test_layer_map_is_identical_across_public_docs() {
    let lib = read_doc("src/lib.rs");
    let readme = read_doc("README.md");
    let guide = read_doc("docs/USER_GUIDE.md");

    let mut lib_order = Vec::new();
    let mut readme_order = Vec::new();
    let mut guide_order = Vec::new();

    for (offset, name) in CANONICAL_LAYERS.iter().enumerate() {
        let n = offset + 1;
        let tagline = CANONICAL_TAGLINES[offset];

        // src/lib.rs: `//! * **Layer N (feature qualifiers) — Name:** body`
        let lib_prefix = format!("//! * **Layer {n} ");
        let (index, line) = find_layer_line(&lib, &lib_prefix, "src/lib.rs");
        assert!(
            line.contains(&format!(" — {name}:**")),
            "TRIPWIRE FIRED: src/lib.rs Layer {n} must be named '{name}' \
             ('... — {name}:**'), found: {line}"
        );
        lib_order.push(index);

        // README.md: `  * **Layer N: Name:** body`
        let readme_prefix = format!("  * **Layer {n}: {name}:**");
        readme_order.push(find_layer_line(&readme, &readme_prefix, "README.md").0);

        // docs/USER_GUIDE.md: `## Layer N: Name (details)`
        let guide_prefix = format!("## Layer {n}: {name}");
        guide_order.push(find_layer_line(&guide, &guide_prefix, "docs/USER_GUIDE.md").0);

        for (doc, text, index) in [
            ("src/lib.rs", &lib, index),
            ("README.md", &readme, *readme_order.last().unwrap()),
            ("docs/USER_GUIDE.md", &guide, *guide_order.last().unwrap()),
        ] {
            let window: Vec<&str> = text.lines().skip(index).take(4).collect();
            assert!(
                window.iter().any(|l| l.contains(tagline)),
                "TRIPWIRE FIRED: {doc} Layer {n} must carry the canonical tagline \
                 {tagline:?} within four lines of its heading; found: {window:?}"
            );
        }
    }

    for (doc, order) in [
        ("src/lib.rs", &lib_order),
        ("README.md", &readme_order),
        ("docs/USER_GUIDE.md", &guide_order),
    ] {
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "TRIPWIRE FIRED: {doc} lists layers out of L1–L5 order: {order:?}"
        );
    }
}

/// The quick-reference table at the end of docs/USER_GUIDE.md has a
/// `Layer` column that must agree with the chapter map of the same
/// guide: each row's layer is the guide chapter that owns the API plus
/// what the row requires (`core`/`alloc`/`std`/`cold`/`contended`).
/// This pins the corrected mappings so the column cannot rot back to
/// stale draft numbers (a row may gain detail, but never move chapters
/// silently).
#[test]
fn test_quick_ref_layer_column_matches_chapters() {
    let guide = read_doc("docs/USER_GUIDE.md");
    const HEADER: &str = "| Operation | Layer | Cost / Footprint | When to Use |";
    let header = guide
        .find(HEADER)
        .expect("quick-reference table header missing from docs/USER_GUIDE.md");
    let table: Vec<&str> = guide[header..]
        .lines()
        .skip(2) // header + separator
        .take_while(|line| line.starts_with('|'))
        .collect();

    let required: &[(&str, &str)] = &[
        ("`snub_check!(cond, reason)`", "1 / core"),
        ("classify_stall(wall, cpu, wb, cb)", "1.5 / core"),
        ("WorkerPulse::kick", "1.5 / core"),
        ("IsrBridge::try_push", "1.5 / core"),
        ("`SafeLockExt`", "1 / std"),
        ("`node.enter_auto` |", "2 / std"),
        ("gateway::<T>(cap)", "2 / std"),
        ("ClosurePolicy::new", "2 / alloc"),
        ("`TaskConduit` / `TaskHandle`", "2 / std"),
        ("ConflateSender::pair()", "4 / std"),
        ("ExecutionEnvelope::new(..)", "4 / std"),
        ("ExecutionEnvelope::pad_to_floor(us)", "4 / core"),
        ("ProfileHarness` + `HarnessReport", "4 / std"),
        ("node.note_memory_kb", "3 / std"),
        ("node.enter_async_auto` |", "3 / std"),
        ("guard.settle_async", "3 / std"),
        ("node.rollback_phase()", "1 / std"),
        ("node.rollback_budget_ms()", "1 / std"),
        ("node.builder(..).rollback_budget_ms(n)", "1 / std"),
        ("RollbackReentrancy::{Allow, Refuse}", "1 / std"),
        ("snubber::watchdog::poll_rollback_node(&node)", "1 / std"),
        (
            "snubber::watchdog::poll_rollback(&node, budget_ms)",
            "1 / std",
        ),
        ("Backpressure::jittered(pct, salt)", "2 / core"),
        ("AimdLimit::new(init, min, max, step, keep_pct)", "2 / core"),
        ("MemoryPolicy::new", "4 / std"),
        ("MemoryTrend<N>::sample(used_kb)", "4 / core"),
        ("HandleWatch<N>::sample(probe)", "4 / core"),
        ("Supervisor::run_supervised_guard_with_memory", "4 / std"),
        ("`Rotation::new(policy)`", "4 / std"),
        ("`Rotation::handoff(&drain, &mut clock)`", "4 / std"),
        ("Supervisor::spawn_successor", "4 / std"),
        ("`PipeDrain` + `ProcessToken`", "4 / std"),
        ("lane.attach", "4 / std"),
        ("DEFAULT_EOF_GRACE_MS` (250 ms)", "4 / std"),
        ("run_supervised_guard_with_grace(worker_flag", "4 / std"),
        ("`NodeSet` set", "5 / cold"),
        (
            "`RankedSet<CAP>::insert(RankedNode::<R>::new(node))",
            "5 / cold",
        ),
        (
            "FlightTape<N>::record_code(epoch, task_id, code)",
            "5 / core",
        ),
        ("write_triage(&mut out, node, &snap)", "5 / cold"),
    ];

    for (prefix, want) in required {
        let mut hits = table.iter().filter(|line| line.contains(prefix));
        let row = hits
            .next()
            .unwrap_or_else(|| panic!("quick-reference row {prefix} missing"));
        assert!(
            hits.next().is_none(),
            "quick-reference row {prefix} appears more than once"
        );
        let layer = row
            .split(" | ")
            .nth(1)
            .unwrap_or_else(|| panic!("quick-reference row {prefix} has no Layer cell: {row}"));
        assert_eq!(
            layer, *want,
            "TRIPWIRE FIRED: quick-reference row {prefix} claims layer '{layer}', \
             the chapter map says '{want}' — fix the row or move the API"
        );
    }
}

/// Parses `[0-9]+(\.[0-9]+)?` at the start of `chars`.
/// Returns `(value, decimal count, chars consumed)`.
fn parse_leading_number(chars: &[char]) -> Option<(f64, u32, usize)> {
    let mut end = 0;
    while end < chars.len() && chars[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 {
        return None;
    }
    let mut consumed = end;
    let mut decimals = 0u32;
    let after = end + 1;
    if end < chars.len()
        && chars[end] == '.'
        && after < chars.len()
        && chars[after].is_ascii_digit()
    {
        consumed = after;
        while consumed < chars.len() && chars[consumed].is_ascii_digit() {
            consumed += 1;
        }
        decimals = (consumed - after) as u32;
    }
    let digits: String = chars[..consumed].iter().collect();
    digits
        .parse::<f64>()
        .ok()
        .map(|value| (value, decimals, consumed))
}

/// Reads an `ns` / `us` / `µs` unit at the head of `chars` (after optional
/// spaces) and returns `(is_microseconds, chars consumed including the
/// skipped spaces)`.
fn parse_unit(chars: &[char]) -> Option<(bool, usize)> {
    let mut rest = chars;
    let mut skipped = 0usize;
    while !rest.is_empty() && (rest[0] == ' ' || rest[0] == '\u{a0}') {
        rest = &rest[1..];
        skipped += 1;
    }
    if rest.starts_with(&['n', 's']) {
        Some((false, skipped + 2))
    } else if rest.starts_with(&['\u{b5}', 's']) || rest.starts_with(&['u', 's']) {
        Some((true, skipped + 2))
    } else {
        None
    }
}

/// One `~<number> <unit>` latency citation found in a doc.
struct Citation {
    value: f64,
    decimals: u32,
    micros: bool,
    raw: String,
    /// `Some(reason)` when the token is not a raw Criterion number:
    /// qualitative `~0`, or a derived per-frame / per-lane division.
    skip: Option<&'static str>,
}

/// Extracts every tilde citation from a doc. Derived forms are marked, not
/// dropped silently: the `~<number> ns/frame` and `~<number> ns per lane`
/// tokens are the same committed measurement divided by a stated
/// frame/lane count, and the all-zero token is a qualitative branch claim.
fn tilde_citations(text: &str) -> Vec<Citation> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '~' {
            i += 1;
            continue;
        }
        let Some((value, decimals, consumed)) = parse_leading_number(&chars[i + 1..]) else {
            i += 1;
            continue;
        };
        let unit_at = i + 1 + consumed;
        let Some((micros, unit_len)) = parse_unit(&chars[unit_at..]) else {
            i += 1;
            continue;
        };
        let end = unit_at + unit_len;
        let raw: String = chars[i..end].iter().collect();
        let window: String = chars[end..(end + 30).min(chars.len())].iter().collect();
        let skip = if raw.chars().filter(|c| c.is_ascii_digit()).all(|b| b == '0') {
            Some("qualitative ~0 claim")
        } else if window.contains("/frame") {
            Some("derived per-frame division")
        } else if window.contains("per lane") {
            Some("derived per-lane division")
        } else {
            None
        };
        out.push(Citation {
            value,
            decimals,
            micros,
            raw,
            skip,
        });
        i = end;
    }
    out
}

/// True when `c` is a faithful rounding or truncation of measurement `m`
/// (both in the citation's own unit). Only the citation's own decimal count
/// is tried: a one-decimal citation must NOT match a measurement whose
/// one-decimal rounding differs, even if their zero-decimal roundings
/// agree — decimal-agnostic matching would let a mis-scaled citation
/// sail through.
fn citation_matches(c: &Citation, m: f64, m_micros: bool) -> bool {
    let m = if c.micros == m_micros {
        m
    } else if c.micros {
        m / 1000.0
    } else {
        m * 1000.0
    };
    let scale = 10f64.powi(c.decimals as i32);
    let rounded = (m * scale + 0.5).floor() / scale;
    let truncated = (m * scale).trunc() / scale;
    (c.value - rounded).abs() < 1e-9 || (c.value - truncated).abs() < 1e-9
}

/// Committed measurements: `**<number> <unit>**` cells of the bench
/// evidence artifact (`(value, is_microseconds, raw)` per row).
fn artifact_measurements(text: &str) -> Vec<(f64, bool, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] != '*' || chars[i + 1] != '*' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let Some((value, _decimals, consumed)) = parse_leading_number(&chars[start..]) else {
            i += 1;
            continue;
        };
        let Some((micros, unit_len)) = parse_unit(&chars[start + consumed..]) else {
            i += 1;
            continue;
        };
        let end = start + consumed + unit_len;
        if end + 1 < chars.len() && chars[end] == '*' && chars[end + 1] == '*' {
            out.push((value, micros, chars[start..end].iter().collect::<String>()));
            i = end + 2;
        } else {
            i += 1;
        }
    }
    out
}

/// The committed bench evidence the doc numbers are checked against. One
/// constant, two oracles: BENCHMARKS.md used to carry its own copy of the
/// path in prose and `doc_sync` carried it twice, so a refreshed snapshot
/// could be pointed at by one reader and missed by the other.
const BENCH_ARTIFACT: &str = "docs/evidence/2026-10-30_bench_snapshot.md";

/// Measured latencies have exactly one home in the public docs:
/// BENCHMARKS.md. Every `~<number>` citation there must round or truncate
/// from the committed bench evidence artifact — no doc carries an
/// independent copy of the numbers. A second rule keeps it that way:
/// README.md and docs/USER_GUIDE.md must quote no latency figure at all,
/// only a link. The two BENCHMARKS.md sections that cite the same artifact
/// row must also share a value, so one section cannot be refreshed alone.
#[test]
fn test_bench_numbers_match_evidence_artifact() {
    let artifact = BENCH_ARTIFACT;
    let rows = artifact_measurements(&read_doc(artifact));
    assert!(
        rows.len() >= 40,
        "artifact parser found only {} measurement rows in {artifact} — the \
         committed table shape changed and this oracle is going vacuously green",
        rows.len()
    );

    let bench_doc = read_doc("BENCHMARKS.md");

    // The doc must point at the very file this oracle reads. Two paths that
    // drift mean a reader checks numbers against one snapshot while the gate
    // checks them against another.
    assert!(
        bench_doc.contains(artifact),
        "TRIPWIRE FIRED: BENCHMARKS.md does not link {artifact} — the artifact \
         this oracle reads and the one the reader is sent to must be one file"
    );

    let mut checked = 0usize;
    let citations = tilde_citations(&bench_doc);
    for c in &citations {
        if c.skip.is_some() {
            continue;
        }
        assert!(
            rows.iter()
                .any(|(m, m_micros, _)| citation_matches(c, *m, *m_micros)),
            "TRIPWIRE FIRED: BENCHMARKS.md cites '{}' but no row of {} \
             rounds or truncates to it. Re-run `cargo bench`, refresh the \
             artifact from target/bench/snapshot.md, and update every \
             BENCHMARKS.md citation to the artifact value — never the \
             artifact to the docs.",
            c.raw,
            artifact
        );
        checked += 1;
    }
    assert!(
        checked >= 40,
        "only {checked} bench citations were checked in BENCHMARKS.md — \
         the token scan broke (units or table shapes renamed?) and \
         a green result here means nothing"
    );

    // The numbers must not migrate back: other public docs link to
    // BENCHMARKS.md and quote no latency figure themselves. The guide's
    // quick reference carries structural costs (footprints, lock and
    // atomic counts) — those parse as no citation, and any measured
    // `~<number> ns|µs` value in it fails here.
    for doc in ["README.md", "docs/USER_GUIDE.md"] {
        let stolen: Vec<String> = tilde_citations(&read_doc(doc))
            .into_iter()
            .filter(|c| c.skip.is_none())
            .map(|c| c.raw)
            .collect();
        assert!(
            stolen.is_empty(),
            "TRIPWIRE FIRED: {doc} quotes latency figures {stolen:?} — \
             measured numbers live in BENCHMARKS.md only; replace them \
             with a link to it"
        );
    }
}

/// The oracle above proves a number is REAL SOMEWHERE in the artifact; it does
/// not prove the number belongs to the row that quotes it. Measured 2026-10-30:
/// `pool.expired_lanes(lease) (4 lanes)` was published as `~5.4 ns` while the
/// committed artifact reads **516.2 ns** and the suite stayed green, because a
/// different row — `SnubberGuard::new + commit` — measures 5.4 ns. A 96x
/// misquote survives an existence check forever.
///
/// This test pairs each BENCHMARKS.md row with the artifact row it names and
/// requires every citation inside that row to round or truncate from THAT
/// measurement. Rows the artifact has no label for (prose-only rows,
/// structural costs) stay under the existence oracle; contended rows, which
/// the doc lists by their Criterion key, are resolved through the exporter's
/// key-to-label map so they are paired too.
#[test]
fn test_bench_rows_quote_their_own_measurement() {
    let artifact_text = read_doc(BENCH_ARTIFACT);
    let bench_doc = read_doc("BENCHMARKS.md");

    let mut by_label: Vec<(String, f64, bool)> = Vec::new();
    for line in artifact_text.lines().filter(|line| line.starts_with("| `")) {
        let label = normalize_cell(line.split('|').nth(1).unwrap_or(""));
        if let Some((value, micros, _)) = artifact_measurements(line).into_iter().next() {
            by_label.push((label, value, micros));
        }
    }
    assert!(
        by_label.len() >= 40,
        "row parser found only {} labelled measurements in {BENCH_ARTIFACT} — \
         the artifact table changed shape and this oracle is going vacuously \
         green",
        by_label.len()
    );

    let sh_map = exporter_labels(&read_doc("scripts/bench.sh"), "labels = {", ":");

    let mut paired = 0usize;
    for line in bench_doc.lines().filter(|line| line.starts_with("| `")) {
        let cell = normalize_cell(line.split('|').nth(1).unwrap_or(""));
        let measurement = by_label
            .iter()
            .find(|(label, _, _)| cell.starts_with(label))
            .map(|(_, value, micros)| (*value, *micros))
            // Contended rows are keyed by their Criterion key: the exporter
            // map says which published label that key carries.
            .or_else(|| {
                let label = sh_map
                    .iter()
                    .find(|(key, _)| cell == normalize_cell(key))
                    .map(|(_, label)| normalize_cell(label))?;
                by_label
                    .iter()
                    .find(|(row_label, _, _)| *row_label == label)
                    .map(|(_, value, micros)| (*value, *micros))
            });
        let Some((value, micros)) = measurement else {
            continue;
        };
        for citation in tilde_citations(line) {
            if citation.skip.is_some() {
                continue;
            }
            assert!(
                citation_matches(&citation, value, micros),
                "TRIPWIRE FIRED: the BENCHMARKS.md row {cell:?} quotes {} but \
                 its own artifact measurement is {value} — the number came from \
                 a different row. Refresh the row to its own measurement, never \
                 borrow a value that happens to exist elsewhere in the artifact.",
                citation.raw
            );
            paired += 1;
        }
    }
    assert!(
        paired >= 30,
        "only {paired} citations were paired to their own row — the row parser \
         or the table shape broke and a green result here means nothing"
    );
}
/// above matches VALUES only, so a renamed API leaves a stale label that no
/// test notices: measured 2026-10-28 the erased row still said
/// `enter_erased_auto` while `benches/admission_bench.rs` calls
/// `probe_erased_auto` — same ~205 ns, opposite lifetime (the probe releases
/// its admission slot at entry; the holding path keeps it until settle).
#[test]
fn test_bench_erased_row_names_the_api_the_bench_calls() {
    let bench_doc = read_doc("BENCHMARKS.md");
    let bench_src = read_doc("benches/admission_bench.rs");
    assert!(
        bench_src.contains("probe_erased_auto"),
        "the erased bench no longer calls probe_erased_auto — update this pin \
         together with the bench, never the doc alone"
    );
    assert!(
        bench_doc.contains("| `probe_erased_auto` + `commit` |"),
        "TRIPWIRE FIRED: the erased row must be labelled with the API the \
         bench calls (`probe_erased_auto`), not a sync/holding namesake"
    );
    for stale in ["`enter_erased_auto`", "`enter_holding_auto`"] {
        assert!(
            !bench_doc.contains(stale),
            "TRIPWIRE FIRED: {stale} is not what any bench measures — \
             `enter_erased_auto` holds the slot for the whole transaction and \
             `enter_holding_auto` does not exist; a reader would benchmark the \
             wrong lifetime"
        );
    }
}

/// The erased row was ONE instance of a class: the label of every other row
/// was pinned by nothing either, and the two exporters that render
/// `target/bench/snapshot.md` carried their own, third copy of every label.
/// This gate closes the class:
///
/// 1. `scripts/bench.sh` and `scripts/bench.ps1` publish the SAME 50+ labels
///    (a fix applied to one exporter only is a drift).
/// 2. Every literal `bench_function("key")` in the bench source is mapped, so
///    no row is published under a bare snake_case key.
/// 3. For each mapped bench, its closure CALLS at least one function whose
///    name appears in its label. This is what fails when an API is renamed
///    but its row is not: the label names something the bench never runs.
/// 4. The committed artifact and BENCHMARKS.md carry that same spelling, so
///    the doc, the exporter and the frozen numbers cannot label one
///    measurement three ways. Contended rows are listed in BENCHMARKS.md by
///    their Criterion key, so only the artifact arm applies to them.
///
/// Not covered: a label may name a real API the closure calls and still
/// describe it badly (`node.enter + commit` for a scoped cycle passes). The
/// gate catches wrong NAMES, not imprecise prose.
#[test]
fn test_bench_labels_name_what_the_bench_calls() {
    let bench_src = read_doc("benches/admission_bench.rs");
    let sh_map = exporter_labels(&read_doc("scripts/bench.sh"), "labels = {", ":");
    let ps_map = exporter_labels(&read_doc("scripts/bench.ps1"), "$labels = @{", "=");
    assert!(
        sh_map.len() >= 50,
        "the exporter label maps shrank to {} entries — the parser or the map broke",
        sh_map.len()
    );
    assert_eq!(
        sh_map, ps_map,
        "TRIPWIRE FIRED: scripts/bench.sh and scripts/bench.ps1 publish different \
         labels for the same Criterion keys; one snapshot table, one spelling"
    );

    let bench_bodies = bench_bodies(&bench_src);
    assert!(
        bench_bodies.len() >= 40,
        "closure parser found only {} literal bench functions — the bench file \
         changed shape and this oracle is going vacuously green",
        bench_bodies.len()
    );

    for (key, _) in &bench_bodies {
        assert!(
            sh_map.iter().any(|(k, _)| k == key),
            "TRIPWIRE FIRED: bench `{key}` is not mapped in either exporter — it \
             would publish under its snake_case function name next to mapped rows"
        );
    }
    let loop_prefixes = format_bench_prefixes(&bench_src);
    assert!(
        !loop_prefixes.is_empty(),
        "no loop-built bench found — the contended group changed shape"
    );

    for (key, body) in &bench_bodies {
        let label = &sh_map
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, l)| l)
            .expect("every literal key is mapped (asserted above)");
        let calls = called_names(body);
        let label_lc = label.to_lowercase();
        assert!(
            calls
                .iter()
                .any(|c| c.len() > 2 && label_lc.contains(&c.to_lowercase())),
            "TRIPWIRE FIRED: bench `{key}` is labelled {label:?}, but its closure \
             calls only {calls:?} — the row names an API the bench never runs \
             (measured 2026-10-28: the erased row said `enter_erased_auto` while \
             the bench called `probe_erased_auto`)"
        );
    }

    let artifact_cells = operation_cells(&read_doc(BENCH_ARTIFACT));
    let doc_cells = operation_cells(&read_doc("BENCHMARKS.md"));
    for (key, label) in &sh_map {
        let wanted = normalize_cell(label);
        assert!(
            artifact_cells.iter().any(|c| c.starts_with(&wanted)),
            "TRIPWIRE FIRED: the committed bench artifact has no row labelled \
             {label:?} (key `{key}`) — the exporter label moved but the snapshot \
             was not refreshed, or the artifact was hand-edited away from it"
        );
        if loop_prefixes.iter().any(|p| key.starts_with(p)) {
            continue;
        }
        // The contended table lists a row by its Criterion key instead of the
        // exporter label; that is still one row for one bench, spelled once.
        assert!(
            doc_cells
                .iter()
                .any(|c| c.starts_with(&wanted) || c.starts_with(key.as_str())),
            "TRIPWIRE FIRED: BENCHMARKS.md has no row labelled {label:?} (key \
             `{key}`) — either add the row or rename it to the exporter's \
             spelling; a reader cannot match the doc to the artifact otherwise"
        );
    }
}

/// `<key> -> <label>` pairs from an exporter's label map, parsed without a
/// regex crate: the block runs from `opener` to the first line that is `}`.
///
/// Offsets are summed from `split_inclusive('\n')` lengths, not from
/// `lines()` plus a hard-coded `+ 1`: `lines()` strips `\r`, so that sum
/// walks backwards one byte per line on a CRLF checkout, cuts the block
/// short and drops the map's tail — the two exporters then disagree.
fn exporter_labels(src: &str, opener: &str, sep: &str) -> Vec<(String, String)> {
    let start = src
        .find(opener)
        .unwrap_or_else(|| panic!("export label map `{opener}` missing"));
    let block = &src[start + opener.len()..];
    let mut end = None;
    let mut offset = 0usize;
    for line in block.split_inclusive('\n') {
        if line.trim() == "}" {
            end = Some(offset);
            break;
        }
        offset += line.len();
    }
    let end = end.unwrap_or_else(|| panic!("export label map `{opener}` never closes"));
    let mut out = Vec::new();
    for line in block[..end].lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((key, rest)) = rest.split_once('"') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix(sep) else {
            continue;
        };
        let Some(rest) = rest.trim().strip_prefix('"') else {
            continue;
        };
        let Some((label, _)) = rest.split_once('"') else {
            continue;
        };
        out.push((key.to_string(), label.to_string()));
    }
    out
}

/// The label parser must give the same map on a CRLF checkout as on LF;
/// GitHub's Windows runner checks the tree out with CRLF while an LF host
/// stays green. The fixture carries enough rows that a one-byte drift per
/// line drops the last row whole: a two-row map only loses punctuation, and
/// the parser tolerates a missing trailing comma, so it cannot see the
/// defect at all.
#[test]
fn exporter_labels_read_a_crlf_checkout() {
    const ROWS: [(&str, &str); 6] = [
        ("a", "alpha"),
        ("b", "beta"),
        ("c", "charlie"),
        ("d", "delta"),
        ("e", "echo"),
        ("f", "foxtrot"),
    ];
    fn map(eol: &str) -> String {
        let mut src = String::from("labels = {");
        src.push_str(eol);
        for (key, label) in ROWS {
            src.push_str(&format!("    \"{key}\" = \"{label}\","));
            src.push_str(eol);
        }
        src.push('}');
        src.push_str(eol);
        src
    }
    let want: Vec<(String, String)> = ROWS
        .iter()
        .map(|(key, label)| (key.to_string(), label.to_string()))
        .collect();
    assert_eq!(exporter_labels(&map("\n"), "labels = {", "="), want);
    assert_eq!(
        exporter_labels(&map("\r\n"), "labels = {", "="),
        want,
        "TRIPWIRE FIRED: a CRLF checkout truncates the exporter label map"
    );
}

/// `(key, closure body)` for every `bench_function("key", |b| { … })`, found
/// by brace matching that skips string literals (bodies contain `{:?}`).
fn bench_bodies(src: &str) -> Vec<(String, String)> {
    const CALL: &str = "bench_function(\"";
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(hit) = src[pos..].find(CALL) {
        let key_start = pos + hit + CALL.len();
        let Some(key_len) = src[key_start..].find('"') else {
            break;
        };
        let key = &src[key_start..key_start + key_len];
        let Some(pipe) = src[key_start..].find("|b|") else {
            break;
        };
        let pipe = key_start + pipe;
        let Some(brace) = src[pipe..].find('{') else {
            break;
        };
        let brace = pipe + brace;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        for (off, ch) in src[brace..].char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(brace + off);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        out.push((key.to_string(), src[brace..=end].to_string()));
        pos = end;
    }
    out
}

/// Literal prefixes of the loop-built bench names (`format!("key_{n}")`).
fn format_bench_prefixes(src: &str) -> Vec<String> {
    const FMT: &str = "format!(\"";
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(hit) = src[pos..].find(FMT) {
        let start = pos + hit + FMT.len();
        let Some(open) = src[start..].find('{').map(|i| start + i) else {
            break;
        };
        let prefix = &src[start..open];
        if !prefix.is_empty() {
            out.push(prefix.to_string());
        }
        pos = open;
    }
    out
}

/// Identifiers immediately followed by `(`, wherever they are called.
fn called_names(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out: Vec<String> = Vec::new();
    for (i, ch) in chars.iter().enumerate() {
        if *ch != '(' || i == 0 {
            continue;
        }
        let mut j = i;
        while j > 0 && (chars[j - 1].is_ascii_alphanumeric() || chars[j - 1] == '_') {
            j -= 1;
        }
        if j == i {
            continue;
        }
        let name: String = chars[j..i].iter().collect();
        if name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    out
}

/// First cell of every markdown table row, backticks stripped, arrow folded
/// to ASCII, whitespace collapsed — comparable across the three files.
fn operation_cells(doc: &str) -> Vec<String> {
    doc.lines()
        .filter(|line| line.starts_with("| "))
        .filter_map(|line| line.split('|').nth(1))
        .map(normalize_cell)
        .filter(|cell| {
            !cell.is_empty() && cell != "Operation" && cell.chars().any(|c| c.is_alphanumeric())
        })
        .collect()
}

fn normalize_cell(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut pending_space = false;
    for ch in cell.chars() {
        if ch == '`' {
            continue;
        }
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        match ch {
            '→' => out.push_str("->"),
            other => out.push(other),
        }
    }
    out
}

/// Body and declared names of a `## <version>` CHANGELOG section: the body has
/// every `<!-- api-breaks ... -->` block stripped, and the names come from the
/// first such block (one token per line, `#` comments and blanks skipped).
/// Stripping matters: a name that only appears inside the block would otherwise
/// satisfy a `contains` check against the section it lives in.
fn changelog_section(changelog: &str, version: &str) -> Option<(String, Vec<String>)> {
    let heading = format!("## {version}");
    let start = changelog.find(&heading)?;
    let after_heading = changelog[start + heading.len()..].chars().next();
    if !matches!(after_heading, None | Some(' ' | '—' | '-')) {
        return None;
    }
    let after = start + heading.len();
    let end = changelog[after..]
        .find("\n## ")
        .map(|n| after + n)
        .unwrap_or(changelog.len());
    let section = &changelog[start..end];

    // Line-oriented: a block opens only on a line that STARTS with `<!--` and
    // names api-breaks, and closes on a line that starts with `-->`. Prose may
    // mention the marker inline (it does), and a substring scan would swallow
    // the rest of the section looking for a terminator.
    let mut body = String::with_capacity(section.len());
    let mut names = Vec::new();
    let mut in_block = false;
    for line in section.split_inclusive('\n') {
        let trimmed = line.trim();
        if in_block {
            if trimmed.starts_with("-->") {
                in_block = false;
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                names.push(trimmed.to_string());
            }
            continue;
        }
        if trimmed.starts_with("<!--") && trimmed.contains("api-breaks") {
            assert!(
                !trimmed.ends_with("-->"),
                "TRIPWIRE FIRED: the api-breaks declaration must be one name per \
                 line inside a multi-line comment — {version} opens and closes it \
                 on the same line, which the release gate cannot read"
            );
            in_block = true;
            continue;
        }
        body.push_str(line);
    }
    assert!(
        !in_block,
        "TRIPWIRE FIRED: the `<!-- api-breaks` block in CHANGELOG.md {version} \
         never closes with `-->`"
    );
    Some((body, names))
}

/// `scripts/check_api_diff.{sh,ps1}` let a deliberate break through only when
/// the removed name is declared. The PUBLISHED channel is the
/// `<!-- api-breaks ... -->` block in the CHANGELOG section for that version
/// (CI checks out the repository and sees it); `api-breaks/<version>.txt` is
/// the local audit copy, gitignored. Three laws:
///
/// 1. A local manifest without a published block is a declaration CI cannot
///    see — the release host would pass a break the runner then fails.
/// 2. When both exist they must name the same set, or the local copy has
///    drifted from what a reader (and the gate on CI) can verify.
/// 3. Every declared name must appear in the CHANGELOG PROSE for the version,
///    or a consumer upgrading reads silence where the break is.
#[test]
fn test_declared_api_breaks_are_in_the_changelog() {
    let cargo = read_doc("Cargo.toml");
    let version = cargo
        .lines()
        .filter_map(|line| line.strip_prefix("version = \""))
        .next()
        .and_then(|rest| rest.split('"').next())
        .expect("Cargo.toml has no package version");

    let changelog = read_doc("CHANGELOG.md");
    let (body, block_names) = changelog_section(&changelog, version).unwrap_or_else(|| {
        panic!(
            "TRIPWIRE FIRED: CHANGELOG.md has no `## {version}` section — the \
             declaration channel and the release gate both look for it"
        )
    });

    let manifest_path = format!("api-breaks/{version}.txt");
    // Absent on CI (the folder is gitignored) — that is the normal case and
    // the published block alone must carry the declaration there.
    let manifest = std::fs::read_to_string(&manifest_path).ok();
    let manifest_names: Vec<String> = manifest
        .as_deref()
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if manifest.is_none() && block_names.is_empty() {
        // No declaration for this version: nothing to reconcile. A release
        // that breaks an API fails Gate 6 instead of passing silently.
        return;
    }
    assert!(
        manifest.is_none() || !manifest_names.is_empty(),
        "TRIPWIRE FIRED: {manifest_path} exists but declares nothing — an empty \
         manifest turns the semver gate into a no-op; delete the file instead"
    );
    assert!(
        !block_names.is_empty(),
        "TRIPWIRE FIRED: {manifest_path} declares breaks but the CHANGELOG \
         `## {version}` section has no `<!-- api-breaks ... -->` block — CI \
         checks out the repository, sees no declaration, and Gate 6 fails red \
         on a change the release host passed. Publish the list with the break."
    );

    if manifest.is_some() {
        let mut sorted_block = block_names.clone();
        sorted_block.sort();
        sorted_block.dedup();
        let mut sorted_manifest = manifest_names.clone();
        sorted_manifest.sort();
        sorted_manifest.dedup();
        assert_eq!(
            sorted_block, sorted_manifest,
            "TRIPWIRE FIRED: the CHANGELOG `<!-- api-breaks -->` block and \
             {manifest_path} name different sets — the published declaration \
             and the audit copy disagree"
        );
    }

    // cargo-semver-checks names a removed item as `Type::method` but a new
    // field as `Type.field`; the CHANGELOG writes paths with `::`. Compare
    // with the separator normalised so the entry still has to be named.
    let body_dotted = body.replace("::", ".");
    for name in &block_names {
        assert!(
            body.contains(name) || body_dotted.contains(&name.replace("::", ".")),
            "TRIPWIRE FIRED: the {version} api-breaks block declares `{name}`, \
             but the section prose never names it — a consumer upgrading reads \
             silence where the break is"
        );
    }
}

/// Any `manual_with_clock` snippet in the public docs must declare the
/// `ClockFn` shape (`fn() -> u64`) in the same fence. `ClockFn` is a fn
/// pointer, not a `u64`; a snippet that shows only the call makes a reader
/// guess the signature through a compiler error.
#[test]
fn test_clock_injection_snippets_declare_the_fn() {
    for doc in ["README.md", "docs/USER_GUIDE.md"] {
        let text = read_doc(doc);
        let mut rest = text.as_str();
        while let Some(pos) = rest.find("```rust") {
            let body = &rest[pos + "```rust".len()..];
            let end = body
                .find("```")
                .unwrap_or_else(|| panic!("{doc}: unterminated rust fence"));
            let fence = &body[..end];
            if fence.contains("manual_with_clock(") {
                assert!(
                    fence.contains(") -> u64 {"),
                    "TRIPWIRE FIRED: {doc} calls manual_with_clock without declaring \
                     `fn ...() -> u64` in the same snippet — ClockFn is a fn pointer, \
                     not a u64 value"
                );
            }
            rest = &body[end + 3..];
        }
    }
}

/// Public docs must not cite SnubberStats methods that the source does not
/// define. The quick reference once sold `success_permille` (the real
/// method is `success_rate_per_mille`), so a reader's autocomplete found
/// nothing; the exporter labels now use the real name, and this tripwire
/// keeps a phantom from creeping back in.
#[test]
fn test_snubberstats_method_names_match_source() {
    let node_src = read_doc("src/node.rs");
    assert!(
        node_src.contains("pub fn success_rate_per_mille"),
        "TRIPWIRE FIRED: src/node.rs lost `success_rate_per_mille` — update \
         the docs and the bench exporter labels together"
    );
    assert!(
        node_src.contains("pub fn shed_total"),
        "TRIPWIRE FIRED: src/node.rs lost `shed_total` — update the docs"
    );
    for doc in ["README.md", "docs/USER_GUIDE.md"] {
        assert!(
            !read_doc(doc).contains("success_permille"),
            "TRIPWIRE FIRED: {doc} cites `success_permille`, a method that does \
             not exist (the real one is `success_rate_per_mille`)"
        );
    }
}

/// Collects `src/...` tokens from prose so a renamed module cannot survive
/// as a stale pointer in the docs.
fn documented_source_paths(text: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("src/") {
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-')))
            .unwrap_or(tail.len());
        let path = tail[..end].trim_end_matches(&['.', '-'][..]);
        if path.len() > "src/".len() {
            paths.push(path);
        }
        rest = &tail[end..];
    }
    paths
}

/// Every module path the public docs cite must exist on disk.
#[test]
fn test_documented_module_paths_exist() {
    for doc in ["src/lib.rs", "README.md", "docs/USER_GUIDE.md"] {
        let text = read_doc(doc);
        for path in documented_source_paths(&text) {
            assert!(
                std::path::Path::new(path).exists(),
                "TRIPWIRE FIRED: {doc} cites '{path}', which does not exist — update the prose"
            );
        }
    }
}

/// The header above names one twin per `rust,no_run` fence. If a fence is
/// added, removed, or moved, this test goes red and the header must follow.
#[test]
fn test_no_run_fences_match_header() {
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    collect_rs(std::path::Path::new("src"), &mut files);

    let mut fences = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read source file");
        for (line_no, line) in text.lines().enumerate() {
            if line.contains("```rust,no_run") {
                fences.push(format!("{}:{}", file.display(), line_no + 1));
            }
        }
    }

    const EXPECTED: usize = 6;
    assert_eq!(
        fences.len(),
        EXPECTED,
        "TRIPWIRE FIRED: found {} `rust,no_run` fences, header claims {EXPECTED}: {fences:?}",
        fences.len(),
    );

    for twin in [
        "tests/supervisor.rs",
        "tests/circuit_breaker.rs",
        "tests/stdin_digest.rs",
        "tests/erased.rs",
        "tests/worker_lifecycle.rs",
        "tests/diagnostics.rs",
        "tests/conduit.rs",
        "tests/scoped_transactions.rs",
    ] {
        assert!(
            std::path::Path::new(twin).exists(),
            "doc twin '{twin}' is gone — update the header twin map",
        );
    }
}

/// The User Guide describes the current state of the crate; release
/// history lives only in CHANGELOG.md (the audit-batch fix tables are
/// the "Audit-batch fix tables" appendix there). A guide that narrates
/// past releases reads stale the day after release, and its anchors rot
/// when the next audit renumbers the batches. Any of the history
/// constructs below reappearing in the guide fails the suite.
#[test]
fn test_guide_has_no_release_history() {
    let guide = read_doc("docs/USER_GUIDE.md");

    let forbidden: &[(&str, &str)] = &[
        (
            "## Fixed in 0.",
            "release-history heading — the audit tables live in the CHANGELOG appendix",
        ),
        (
            "## Fixed after 0.",
            "release-history heading — the audit tables live in the CHANGELOG appendix",
        ),
        (
            "## Extensibility (0.",
            "version-labelled heading — the chapter map holds without the version tag",
        ),
        (
            "> Hang layer (v0.",
            "header teaser narrating an old release — Layer 1.5 documents the current hang layer",
        ),
        (
            "> Extensibility (v0.",
            "header teaser narrating an old release — the Extensibility chapter documents the current surface",
        ),
        (
            "CHANGELOG",
            "a mention of the changelog file — the guide describes the current state and names no change records",
        ),
    ];

    for (token, why) in forbidden {
        assert!(
            !guide.contains(token),
            "TRIPWIRE FIRED: docs/USER_GUIDE.md carries release history ({token}…): {why}. \
             The guide describes the current state; move history to CHANGELOG.md"
        );
    }
}

/// GitHub-slug of a heading, matching github-slugger: lowercase, drop
/// every character that is not a letter, digit, `_`, `-` or space, then
/// replace each space with `-`. Verified against the rendered anchors of
/// this repo (`&` in a heading yields a double dash: "API & Cost" →
/// `api--cost-quick-reference`).
fn github_slug(heading: &str) -> String {
    let mut out = String::new();
    for ch in heading.trim().to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-' || ch == ' ' {
            out.push(ch);
        }
    }
    out.replace(' ', "-")
}

/// Every internal anchor link in the guide — and every cross-document
/// `USER_GUIDE.md#…` link from README and BENCHMARKS — must resolve to a
/// heading that exists in docs/USER_GUIDE.md. The quick-diagnosis table
/// and the "why the shape" prose are full of `#anchors`; one renamed
/// heading must break the test, not silently rot into a dead link.
#[test]
fn test_guide_anchors_resolve_to_headings() {
    let guide = read_doc("docs/USER_GUIDE.md");
    let slugs: std::collections::BTreeSet<String> = guide
        .lines()
        .filter(|line| line.starts_with('#'))
        .map(|line| github_slug(line.trim_start_matches('#')))
        .collect();
    assert!(
        slugs.len() >= 20,
        "only {} heading slugs parsed from docs/USER_GUIDE.md — heading \
         extraction broke and this oracle is going vacuously green",
        slugs.len()
    );

    let mut checked = 0usize;
    let mut scan = |doc: &str, text: &str| {
        let mut rest = text;
        while let Some(pos) = rest.find("](#") {
            let tail = &rest[pos + 3..];
            let end = tail
                .find(')')
                .unwrap_or_else(|| panic!("{doc}: unterminated anchor link"));
            let anchor = &tail[..end];
            assert!(
                slugs.contains(anchor),
                "TRIPWIRE FIRED: {doc} links to dead anchor '#{anchor}' — \
                 no heading in docs/USER_GUIDE.md renders that slug; \
                 rename the link or restore the heading"
            );
            checked += 1;
            rest = &tail[end + 1..];
        }
    };

    scan("docs/USER_GUIDE.md", &guide);
    for doc in ["README.md", "BENCHMARKS.md"] {
        let text = read_doc(doc);
        // only guide-targeting links: keep the fragment after the path
        let mut rest = text.as_str();
        while let Some(pos) = rest.find("docs/USER_GUIDE.md#") {
            let tail = &rest[pos + "docs/USER_GUIDE.md#".len()..];
            let end = tail
                .find([')', ' ', '"'])
                .unwrap_or_else(|| panic!("{doc}: unterminated guide anchor link"));
            let anchor = &tail[..end];
            assert!(
                slugs.contains(anchor),
                "TRIPWIRE FIRED: {doc} links to dead guide anchor '#{anchor}' — \
                 no heading in docs/USER_GUIDE.md renders that slug; \
                 rename the link or restore the heading"
            );
            checked += 1;
            rest = &tail[end + 1..];
        }
    }
    assert!(
        checked >= 10,
        "only {checked} anchors were checked — the link scan broke and \
         a green result here means nothing"
    );
}

// ── Storm evidence oracle ───────────────────────────────────────────────
//
// Every storm digit in BENCHMARKS.md is re-derived here from the committed
// artifact, and the doc line must match byte for byte. This lives in code
// rather than in a reviewer's eye because a report copied out of `target/`
// and a prose sentence drift the moment one lane is re-run, and the failure
// mode is a published benchmark nobody can reproduce.
//
// No JSON crate: the dev graph stays dependency-free, and the subset needed
// here (objects, arrays, scalars) is a few dozen lines. The reader is
// SCOPED — `field` looks inside one object, never the whole file — because
// `dispatch_us`, `samples`, `p50` and `final_generation` each appear once in
// the aggregate and once per lane. A first-occurrence reader would answer an
// aggregate question with lane 0's number and never say so.

/// Byte span of one value inside an artifact.
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

struct StormDoc {
    path: String,
    text: String,
}

fn storm_skip_ws(t: &str, mut i: usize) -> usize {
    let b = t.as_bytes();
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// Index just past the closing quote of the string starting at `open`.
fn storm_string_end(t: &str, open: usize) -> usize {
    let b = t.as_bytes();
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    panic!("unterminated JSON string at byte {open}")
}

/// Exclusive end of the value starting at `start` (which is not whitespace).
fn storm_value_end(t: &str, start: usize) -> usize {
    let b = t.as_bytes();
    let open = b[start];
    if open == b'"' {
        return storm_string_end(t, start);
    }
    if open == b'{' || open == b'[' {
        let close = if open == b'{' { b'}' } else { b']' };
        let mut i = start + 1;
        while i < b.len() {
            let c = b[i];
            if c == b'"' {
                i = storm_string_end(t, i);
            } else if c == b'{' || c == b'[' {
                i = storm_value_end(t, i);
            } else if c == close {
                return i + 1;
            } else {
                i += 1;
            }
        }
        panic!("unbalanced JSON container at byte {start}");
    }
    let mut i = start;
    while i < b.len()
        && !matches!(
            b[i],
            b',' | b'}' | b']' | b':' | b' ' | b'\t' | b'\r' | b'\n'
        )
    {
        i += 1;
    }
    i
}

/// `12345` -> `12,345`, the grouping every storm token uses.
fn storm_group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Fixed point, half-away-from-zero, grouped integer part. `f64::round` is
/// the whole point: .NET `Math.Round` and Python `round()` are banker's
/// rounding, so a tie such as 74.25 % reads 74.2 there and 74.3 here. The
/// harness prints one thing and this file must quote the same thing.
fn storm_fixed(value: f64, dp: usize) -> String {
    let scale = 10f64.powi(dp as i32);
    let scaled = (value * scale).round();
    let neg = scaled < 0.0;
    let mag = scaled.abs() as u64;
    let whole = mag / scale as u64;
    let frac = mag % scale as u64;
    let body = if dp == 0 {
        storm_group(whole)
    } else {
        format!("{}.{:0>width$}", storm_group(whole), frac, width = dp)
    };
    if neg && mag > 0 {
        format!("-{body}")
    } else {
        body
    }
}

/// A growth of zero reads `+0`: the sign is the claim that the window moved.
fn storm_signed(value: f64, dp: usize) -> String {
    let body = storm_fixed(value, dp);
    if body.starts_with('-') {
        body
    } else {
        format!("+{body}")
    }
}

fn storm_bounds(values: &[f64]) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for v in values {
        if *v < lo {
            lo = *v;
        }
        if *v > hi {
            hi = *v;
        }
    }
    (lo, hi)
}

/// `311.1…318.3 µs`, or a single value when every lane reads the same.
fn storm_range(values: &[f64], dp: usize, unit: &str) -> String {
    let (lo, hi) = storm_bounds(values);
    let (a, b) = (storm_fixed(lo, dp), storm_fixed(hi, dp));
    if a == b {
        format!("{a}{unit}")
    } else {
        format!("{a}…{b}{unit}")
    }
}

fn storm_signed_range(values: &[f64], dp: usize, unit: &str) -> String {
    let (lo, hi) = storm_bounds(values);
    let (a, b) = (storm_signed(lo, dp), storm_signed(hi, dp));
    if a == b {
        format!("{a}{unit}")
    } else {
        format!("{a}…{b}{unit}")
    }
}

/// `10,000 (100.0%)`, or a bare `0` — an empty bucket is a fact, not a rate.
fn storm_cell(count: u64, total: u64) -> String {
    if count == 0 {
        return "0".to_string();
    }
    let pct = 100.0 * count as f64 / total as f64;
    format!("{} ({}%)", storm_group(count), storm_fixed(pct, 1))
}

/// Band edges, identical to `HIST_BUCKETS` / `$histBuckets` in the harnesses.
const STORM_BANDS: [u64; 8] = [50, 100, 200, 500, 1000, 2000, 5000, 10000];

/// `(artifact, table row label, prose label)`
const STORM_RUNS: [(&str, &str, &str); 3] = [
    (
        "docs/evidence/2026-10-31_multi_lineage_armedhold_report.json",
        "`-ArmedHold`",
        "armed-hold",
    ),
    (
        "docs/evidence/2026-10-31_multi_lineage_freerun_report.json",
        "`-Aggressive` (free-running)",
        "free-running",
    ),
    (
        "docs/evidence/2026-10-31_multi_lineage_saturation_report.json",
        "`-ArmedHold` at 8 lanes",
        "saturated",
    ),
];

/// `(artifact, row label, which storm the pool example ran)`
const POOL_RUNS: [(&str, &str, &str); 3] = [
    (
        "docs/evidence/2026-10-30_pool_stress_report.json",
        "`supervisor_pool_stress`",
        "pool",
    ),
    (
        "docs/evidence/2026-10-30_pool_lease_stress_report.json",
        "`supervisor_pool_stress_lease`",
        "lease",
    ),
    (
        "docs/evidence/2026-10-30_pool_mixed_stress_report.json",
        "`supervisor_pool_stress_mixed`",
        "mixed",
    ),
];

impl StormDoc {
    fn load(path: &str) -> StormDoc {
        // Windows PowerShell writes its report with a BOM; it is not JSON.
        let text = read_doc(path).trim_start_matches('\u{feff}').to_string();
        StormDoc {
            path: path.to_string(),
            text,
        }
    }

    fn byte(&self, i: usize) -> u8 {
        *self
            .text
            .as_bytes()
            .get(i)
            .unwrap_or_else(|| panic!("{}: JSON ended at byte {i}", self.path))
    }

    fn expect(&self, i: usize, want: u8) {
        if self.byte(i) != want {
            panic!(
                "{}: expected `{}` at byte {i}, found `{}` — the report shape \
                 changed; refresh docs/evidence and this oracle together",
                self.path,
                want as char,
                self.byte(i) as char
            );
        }
    }

    fn root(&self) -> Span {
        let start = storm_skip_ws(&self.text, 0);
        self.expect(start, b'{');
        Span {
            start,
            end: storm_value_end(&self.text, start),
        }
    }

    fn slice(&self, s: Span) -> &str {
        &self.text[s.start..s.end]
    }

    fn members(&self, obj: Span) -> Vec<(String, Span)> {
        let mut out = Vec::new();
        let mut i = storm_skip_ws(&self.text, obj.start + 1);
        if self.byte(i) == b'}' {
            return out;
        }
        loop {
            self.expect(i, b'"');
            let key_end = storm_string_end(&self.text, i);
            let key = self.text[i + 1..key_end - 1].to_string();
            i = storm_skip_ws(&self.text, key_end);
            self.expect(i, b':');
            let value = storm_skip_ws(&self.text, i + 1);
            let end = storm_value_end(&self.text, value);
            out.push((key, Span { start: value, end }));
            i = storm_skip_ws(&self.text, end);
            match self.byte(i) {
                b',' => i = storm_skip_ws(&self.text, i + 1),
                b'}' => break,
                other => panic!(
                    "{}: expected `,` or `}}` at byte {i}, found `{}`",
                    self.path, other as char
                ),
            }
        }
        out
    }

    fn items(&self, arr: Span) -> Vec<Span> {
        let mut out = Vec::new();
        let mut i = storm_skip_ws(&self.text, arr.start + 1);
        if self.byte(i) == b']' {
            return out;
        }
        loop {
            let value = storm_skip_ws(&self.text, i);
            let end = storm_value_end(&self.text, value);
            out.push(Span { start: value, end });
            i = storm_skip_ws(&self.text, end);
            match self.byte(i) {
                b',' => i = storm_skip_ws(&self.text, i + 1),
                b']' => break,
                other => panic!(
                    "{}: expected `,` or `]` at byte {i}, found `{}`",
                    self.path, other as char
                ),
            }
        }
        out
    }

    fn field(&self, obj: Span, key: &str) -> Span {
        self.members(obj)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or_else(|| {
                panic!(
                    "{}: this object has no key `{key}`. The harness report \
                     shape changed; refresh docs/evidence and this oracle \
                     together, never the docs alone.",
                    self.path
                )
            })
    }

    fn nested(&self, obj: Span, outer: &str, key: &str) -> Span {
        self.field(self.field(obj, outer), key)
    }

    fn num(&self, s: Span) -> f64 {
        self.slice(s).parse::<f64>().unwrap_or_else(|err| {
            panic!(
                "{}: `{}` is not a number ({err}) — the report is corrupt or truncated",
                self.path,
                self.slice(s)
            )
        })
    }

    fn usz(&self, s: Span) -> u64 {
        self.slice(s).parse::<u64>().unwrap_or_else(|err| {
            panic!(
                "{}: `{}` is not an integer ({err}) — a count key was asked for a fractional value",
                self.path,
                self.slice(s)
            )
        })
    }

    fn flag(&self, s: Span) -> bool {
        match self.slice(s) {
            "true" => true,
            "false" => false,
            other => panic!("{}: `{other}` is neither true nor false", self.path),
        }
    }

    fn text_of(&self, s: Span) -> String {
        let raw = self.slice(s);
        if raw == "null" {
            return String::new();
        }
        raw[1..raw.len() - 1].to_string()
    }

    fn is_null(&self, s: Span) -> bool {
        self.slice(s) == "null"
    }

    /// One number per lane, out of `outer.key`.
    fn per_lane(&self, lanes: &[Span], outer: &str, key: &str) -> Vec<f64> {
        lanes
            .iter()
            .map(|l| {
                if outer.is_empty() {
                    self.num(self.field(*l, key))
                } else {
                    self.num(self.nested(*l, outer, key))
                }
            })
            .collect()
    }

    fn sum_per_lane(&self, lanes: &[Span], outer: &str, key: &str) -> u64 {
        lanes
            .iter()
            .map(|l| {
                if outer.is_empty() {
                    self.usz(self.field(*l, key))
                } else {
                    self.usz(self.nested(*l, outer, key))
                }
            })
            .sum()
    }
}

/// Fail with the artifact, the expected line and the rule, never a bare assert.
fn storm_assert_line(doc: &str, line: &str, artifact: &str, kind: &str) {
    let want = line.trim();
    assert!(
        doc.lines().any(|have| have.trim() == want),
        "TRIPWIRE FIRED: BENCHMARKS.md carries no {kind} line matching {artifact}.\n  \
         expected: {want}\n  \
         Every storm number has exactly one source: the committed report. Re-run the \
         storm, refresh the artifact, regenerate the doc from it — never the artifact \
         from the doc.",
    );
}

fn storm_assert_token(doc: &str, token: &str, artifact: &str) {
    assert!(
        doc.contains(token),
        "TRIPWIRE FIRED: BENCHMARKS.md lost the token '{token}' that {artifact} still \
         carries. Refresh the artifact, then update every storm citation to the \
         artifact value.",
    );
}

/// The termination-storm tables in BENCHMARKS.md quote the committed
/// multi-lineage reports. Every headline token, per-lane range, band count,
/// machine-identity fragment and rescue counter is re-derived from the
/// artifact here, and the artifact is checked against itself first: a doc
/// that quotes a self-inconsistent report is broken twice over.
#[test]
fn test_storm_tables_match_committed_reports() {
    let bench_doc = read_doc("BENCHMARKS.md");
    for stale in ["2026-10-27_multi_lineage", "2026-10-30_multi_lineage"] {
        assert!(
            !bench_doc.contains(stale),
            "BENCHMARKS.md still cites the {stale} storm reports as current \
             storm evidence; the set named in STORM_RUNS supersedes them"
        );
    }
    // The numbers may be re-derived without the doc saying where they came
    // from, so pin the citation too: every artifact this oracle reads must be
    // linked, and a doc that quotes an unlinked report is quoting a file no
    // reader can open.
    for (path, row_label, _) in STORM_RUNS {
        assert!(
            bench_doc.contains(path),
            "BENCHMARKS.md does not link {path} (the `{row_label}` row), so the row and \
             the artifact this oracle reads are not the same run"
        );
    }
    let mut checked = 0usize;

    for (path, row_label, run_label) in STORM_RUNS {
        let doc = StormDoc::load(path);
        let root = doc.root();

        assert_eq!(
            doc.text_of(doc.field(root, "verdict")),
            "PASS",
            "{path}: committed evidence must carry a PASS verdict"
        );
        assert_eq!(
            doc.usz(doc.field(root, "report_schema")),
            2,
            "{path}: report_schema is not 2, so the disjoint `band_*` keys \
             this oracle reads are not the keys the harness writes"
        );
        assert_eq!(doc.text_of(doc.field(root, "model")), "multi_lineage");
        assert_eq!(
            doc.text_of(doc.nested(root, "system", "profile")),
            "release",
            "{path}: a storm figure quoted in BENCHMARKS.md must come from a release build"
        );
        checked += 1;

        // The battery itself: every gate green, and exactly as many gates as
        // the harness writes (11 per lane under -ArmedHold, 10 without the
        // ambush-rate gate, plus 4 or 3 aggregates).
        let armed = doc.flag(doc.field(root, "armed_hold"));
        let lanes = doc.items(doc.field(root, "lanes"));
        let checks = doc.items(doc.field(root, "checks"));
        let per_lane = if armed { 11 } else { 10 };
        let aggregate = if armed { 4 } else { 3 };
        assert_eq!(
            checks.len(),
            lanes.len() * per_lane + aggregate,
            "{path}: {}/{} checks is not lanes x {per_lane} + {aggregate} — a lane \
             gate was added or dropped without moving this oracle",
            checks.len(),
            lanes.len()
        );
        let red: Vec<String> = checks
            .iter()
            .filter(|c| !doc.flag(doc.field(**c, "ok")))
            .map(|c| doc.text_of(doc.field(*c, "name")))
            .collect();
        assert!(
            red.is_empty(),
            "{path}: {red:?} are red in the artifact, so no doc may quote it"
        );
        let readable = checks
            .iter()
            .filter(|c| {
                doc.text_of(doc.field(**c, "name"))
                    .ends_with("resource window readable")
            })
            .count();
        assert_eq!(
            readable,
            lanes.len(),
            "{path}: only {readable} of {} lanes report a readable resource window",
            lanes.len()
        );
        // The count formula cannot say *which* gate the harness wrote, so name
        // the shape gate: a lane that silently lost `-ArmedHold` would still
        // emit the right number of checks, and armed-hold is the row the
        // benchmarks quote.
        let shape = checks
            .iter()
            .filter(|c| {
                doc.text_of(doc.field(**c, "name"))
                    .ends_with("armed_hold matches flag")
            })
            .count();
        assert_eq!(
            shape,
            lanes.len(),
            "{path}: only {shape} of {} lanes carry the `armed_hold matches flag` gate — \
             the harness stopped stamping the shape it actually ran",
            lanes.len()
        );

        let kills_per_line = doc.usz(doc.field(root, "kills_per_line"));
        let total = doc.usz(doc.field(root, "kills_completed"));
        assert_eq!(doc.usz(doc.field(root, "lines")), lanes.len() as u64);
        assert_eq!(
            doc.usz(doc.field(root, "kills_requested")),
            total,
            "{path}: a lane ended short of its dispatch count"
        );
        assert_eq!(doc.usz(doc.nested(root, "dispatch_us", "count")), total);

        let killed = doc.sum_per_lane(&lanes, "", "killed");
        assert_eq!(
            killed, total,
            "{path}: the lanes do not account for every kill"
        );
        assert_eq!(
            doc.sum_per_lane(&lanes, "", "stale"),
            0,
            "{path}: a killer shot a recycled pid"
        );
        assert_eq!(doc.usz(doc.nested(root, "killers_saw", "stale")), 0);
        for key in ["duplicates", "retries", "miss_polls"] {
            assert_eq!(
                doc.sum_per_lane(&lanes, "", key),
                doc.usz(doc.field(doc.field(root, "killers_saw"), key)),
                "{path}: aggregate `{key}` is not the sum over lanes"
            );
        }
        // The root bucket is `clean_escape`; the lane bucket is `escape`.
        // Reading one name for both is how a doc ends up quoting a number
        // the harness never wrote.
        for (aggregate, lane_key) in [
            ("true_ambush", "true_ambush"),
            ("clean_escape", "escape"),
            ("stillborn", "stillborn"),
        ] {
            assert_eq!(
                doc.sum_per_lane(&lanes, "classification", lane_key),
                doc.usz(doc.field(root, aggregate)),
                "{path}: the aggregate `{aggregate}` bucket is not the sum of the lanes"
            );
        }
        assert_eq!(
            doc.sum_per_lane(&lanes, "report", "tokens_rescued"),
            doc.usz(doc.field(root, "tokens_rescued")),
            "{path}: tokens_rescued drifted from the lanes that earned them"
        );

        // Rescue accounting is a partition of the kills, not a tally: a token
        // is rescued exactly when the death landed in the armed window, and
        // everything else left through the normal settle path. Together these
        // two equalities are why a high ambush rate cannot be manufactured by
        // dropping tokens — an unsettled kill reads as a missing rescue.
        let rescued = doc.usz(doc.field(root, "tokens_rescued"));
        let settles = doc.usz(doc.field(root, "clean_settles"));
        let ambush = doc.usz(doc.field(root, "true_ambush"));
        assert_eq!(
            rescued, ambush,
            "{path}: {rescued} tokens rescued against {ambush} kills classified as true ambush"
        );
        assert_eq!(
            rescued + settles,
            total,
            "{path}: `tokens_rescued` + `clean_settles` = {} is not the {total} kills completed",
            rescued + settles
        );
        storm_assert_token(
            &bench_doc,
            &format!(
                "`tokens_rescued` {} of {} kills, `clean_settles` {}",
                storm_group(rescued),
                storm_group(total),
                storm_group(settles)
            ),
            path,
        );

        let buckets = doc.field(root, "dispatch_histogram_us");
        let mut band_sum = doc.usz(doc.field(buckets, "band_above_10000"));
        for band in STORM_BANDS {
            band_sum += doc.usz(doc.field(buckets, &format!("band_{band}")));
        }
        assert_eq!(
            band_sum, total,
            "{path}: the nine disjoint bands sum to {band_sum}, not to the {total} kills dispatched"
        );

        let samples = doc.per_lane(&lanes, "resources", "samples");
        let drops = doc.per_lane(&lanes, "resources", "sample_drops");
        let handles_last = doc.per_lane(&lanes, "resources", "handles_last");
        let growth = doc.per_lane(&lanes, "resources", "private_growth_mb");
        let handle_growth = doc.per_lane(&lanes, "resources", "handles_growth");
        let p50 = doc.per_lane(&lanes, "dispatch_us", "p50");
        let p99 = doc.per_lane(&lanes, "dispatch_us", "p99");
        let generations = doc.per_lane(&lanes, "report", "final_generation");
        let exits = doc.per_lane(&lanes, "", "supervisor_exit");

        for (index, lane) in lanes.iter().enumerate() {
            assert_eq!(doc.usz(doc.field(*lane, "index")), index as u64);
            assert_eq!(
                doc.usz(doc.field(*lane, "killed")),
                kills_per_line,
                "{path}: lane {index} reaped a different count than it dispatched"
            );
            assert_eq!(doc.usz(doc.field(*lane, "stale")), 0);
            assert!(
                handles_last[index] > 0.0,
                "{path}: lane {index} published a 0 handle count — the fabricated \
                 reading that fakes a leak drop"
            );
            assert!(
                samples[index] >= 20.0,
                "{path}: lane {index} has {} samples, too few to call its window readable",
                samples[index]
            );
            assert!(
                drops[index] < samples[index],
                "{path}: lane {index} dropped {} of {} polls",
                drops[index],
                samples[index]
            );
            assert_eq!(
                exits[index], 0.0,
                "{path}: lane {index} supervisor exited nonzero"
            );
            let card = doc.field(*lane, "report");
            assert!(
                doc.is_null(doc.field(*lane, "error")),
                "{path}: lane {index} killer reported an error"
            );
            assert!(!doc.flag(doc.field(*lane, "stalled")));
            assert_eq!(doc.usz(doc.field(card, "misclassified_stillborn")), 0);
            assert_eq!(
                generations[index] as u64,
                kills_per_line + 1,
                "{path}: lane {index} bumped its generation {} times for {kills_per_line} deaths",
                generations[index] as u64 - 1
            );
            assert_eq!(
                doc.usz(doc.field(card, "ambush_kills"))
                    + doc.usz(doc.field(card, "escape_kills"))
                    + doc.usz(doc.field(card, "stillborn_kills")),
                doc.usz(doc.field(card, "target_kills")),
                "{path}: lane {index} buckets do not conserve kills"
            );
        }
        if armed {
            let pct = 100.0 * doc.usz(doc.field(root, "true_ambush")) as f64 / total as f64;
            assert!(
                pct >= doc.num(doc.field(root, "min_ambush_pct")),
                "{path}: an armed-hold artifact only proves rescue if it clears its own ambush gate"
            );
        }
        checked += 5;

        // ── the table row, rebuilt from the artifact
        let dispatch = doc.field(root, "dispatch_us");
        let row = format!(
            "| {row_label} | {} × {} | {} s | {} / {} | {} / {} / {} µs | {} | {} | {} |",
            doc.usz(doc.field(root, "lines")),
            storm_group(kills_per_line),
            storm_fixed(doc.num(doc.field(root, "duration_ms")) / 1000.0, 2),
            storm_fixed(doc.num(doc.field(root, "rate_per_sec")), 1),
            storm_fixed(doc.num(doc.field(root, "peak_rate_per_sec")), 1),
            storm_fixed(doc.num(doc.field(dispatch, "p50")), 1),
            storm_fixed(doc.num(doc.field(dispatch, "p99")), 1),
            storm_fixed(doc.num(doc.field(dispatch, "max")), 0),
            storm_cell(doc.usz(doc.field(root, "true_ambush")), total),
            storm_cell(doc.usz(doc.field(root, "clean_escape")), total),
            storm_cell(doc.usz(doc.field(root, "stillborn")), total),
        );
        storm_assert_line(&bench_doc, &row, path, "mode");

        // ── the per-lane bullet, its band line and its killer counters
        let mut parts: Vec<String> = Vec::new();
        let mut prev = 0u64;
        for band in STORM_BANDS {
            let count = doc.usz(doc.field(buckets, &format!("band_{band}")));
            parts.push(format!("{prev}-{band}: {}", storm_group(count)));
            prev = band;
        }
        parts.push(format!(
            ">{prev}: {}",
            storm_group(doc.usz(doc.field(buckets, "band_above_10000")))
        ));
        let band_line = parts.join(" | ");
        let killers = doc.field(root, "killers_saw");
        let bullet = format!(
            "- **{run_label}** ({} lineages × {} kills, wall {} s) — per-lane dispatch p50 {}, p99 {}; private-memory growth {}; handle growth {}; samples {} per lane with {} dropped reads; `final_generation` {} = kills + 1; `supervisor_exit` {}.",
            doc.usz(doc.field(root, "lines")),
            storm_group(kills_per_line),
            storm_fixed(doc.num(doc.field(root, "duration_ms")) / 1000.0, 2),
            storm_range(&p50, 1, " µs"),
            storm_range(&p99, 1, " µs"),
            storm_signed_range(&growth, 2, " MB"),
            storm_signed_range(&handle_growth, 0, ""),
            storm_range(&samples, 0, ""),
            storm_range(&drops, 0, ""),
            storm_range(&generations, 0, ""),
            storm_range(&exits, 0, ""),
        );
        storm_assert_line(&bench_doc, &bullet, path, "per-lane detail");
        storm_assert_line(
            &bench_doc,
            &format!(
                "Bands (µs, disjoint, sum {}): {band_line}.",
                storm_group(total)
            ),
            path,
            "band count",
        );
        storm_assert_line(
            &bench_doc,
            &format!(
                "Killer counters: {} stale pids, {} duplicate pid-file reads, {} retries, {} pid-miss polls across {} lineages.",
                storm_group(doc.usz(doc.field(killers, "stale"))),
                storm_group(doc.usz(doc.field(killers, "duplicates"))),
                storm_group(doc.usz(doc.field(killers, "retries"))),
                storm_group(doc.usz(doc.field(killers, "miss_polls"))),
                doc.usz(doc.field(root, "lines")),
            ),
            path,
            "killer counter",
        );

        // ── the machine identity, read out of the artifact's system block
        let system = doc.field(root, "system");
        let caption = format!(
            "{}, {} physical / {} logical cores, {} build {}, {}, PowerShell {}, {} profile",
            doc.text_of(doc.field(system, "cpu_name")),
            doc.usz(doc.field(system, "physical_cores")),
            doc.usz(doc.field(system, "logical_cores")),
            doc.text_of(doc.field(system, "os_caption")),
            doc.text_of(doc.field(system, "os_build")),
            doc.text_of(doc.field(system, "rustc")),
            doc.text_of(doc.field(system, "ps_version")),
            doc.text_of(doc.field(system, "profile")),
        );
        storm_assert_token(&bench_doc, &caption, path);
        storm_assert_token(&bench_doc, path, path);
        storm_assert_token(&bench_doc, &path.replace("_report.json", "_run.log"), path);
        for cited in [path, &path.replace("_report.json", "_run.log")] {
            assert!(
                std::path::Path::new(cited).exists(),
                "BENCHMARKS.md cites {cited} and this oracle reads it, but it is not in \
                 the tree — the artifact was never committed, so the citation is a link \
                 to nothing"
            );
        }
        checked += 6;
    }

    // The prose that explains the schema has to carry the schema, and the
    // gates this oracle counts by name have to be named in the doc: a bump to
    // `report_schema: 3` that leaves the sentence saying 2 describes a report
    // no harness writes, and a renamed lane gate would silently drop out of
    // the count above.
    storm_assert_token(&bench_doc, "report_schema: 2", "the harness paragraph");
    for gate in ["resource window readable", "sample_drops", "band_200"] {
        storm_assert_token(&bench_doc, gate, "the sampler-coherence paragraph");
    }

    assert!(
        checked >= STORM_RUNS.len() * 12,
        "only {checked} of the {} storm checks ran — the loop broke early, and a \
         green result here would mean nothing",
        STORM_RUNS.len() * 12
    );
}

/// The pool storm runs the same kill/reap loop inside one pool process. Its
/// report is a single line of counters, so the whole doc row is pinned.
#[test]
fn test_pool_storm_tables_match_committed_reports() {
    let bench_doc = read_doc("BENCHMARKS.md");
    assert!(
        !bench_doc.contains("2026-10-26_pool_"),
        "BENCHMARKS.md still cites the 2026-10-26 pool artifacts; the 2026-10-30 set \
         in docs/evidence supersedes them"
    );
    for (path, label, mode) in POOL_RUNS {
        let doc = StormDoc::load(path);
        let root = doc.root();
        let lanes = doc.usz(doc.field(root, "lanes"));
        let reaped = doc.usz(doc.field(root, "reaped"));
        let ambush = doc.usz(doc.field(root, "ambush"));
        let escape = doc.usz(doc.field(root, "escape"));
        let stillborn = doc.usz(doc.field(root, "stillborn"));
        let deadlocks = doc.usz(doc.field(root, "deadlocks"));

        assert!(
            doc.flag(doc.field(root, "conserved")),
            "{path}: the pool report is not conserved"
        );
        let breaks = doc.usz(doc.field(root, "conservation_breaks"));
        assert_eq!(
            breaks, 0,
            "{path}: accounting drifted {breaks} times mid-storm"
        );
        assert_eq!(
            doc.usz(doc.field(root, "token_lane_mismatch")),
            0,
            "{path}: a token was accounted to the wrong lane"
        );
        assert_eq!(
            ambush + escape + stillborn + deadlocks,
            reaped,
            "{path}: the four buckets do not sum to the {reaped} reaps"
        );
        assert_eq!(
            doc.usz(doc.field(root, "rescued")),
            ambush,
            "{path}: rescued != ambush"
        );
        assert_eq!(
            doc.usz(doc.field(root, "tokens_seen")),
            ambush,
            "{path}: a token was seen without an armed kill"
        );
        let generations = doc.items(doc.field(root, "final_generation"));
        assert_eq!(generations.len(), lanes as usize);
        for g in &generations {
            assert_eq!(
                doc.usz(*g),
                reaped / lanes + 1,
                "{path}: a lane bumped {} times for {} deaths",
                doc.usz(*g),
                reaped / lanes
            );
        }
        match mode {
            "lease" => {
                assert_eq!(doc.usz(doc.field(root, "lease_kills")), reaped);
                assert_eq!(
                    stillborn, reaped,
                    "{path}: a heartbeat proves the pipe, not a settlement"
                );
                assert!(
                    doc.usz(doc.field(root, "monitor_checks")) > 0,
                    "{path}: the monitor never read the pool during the storm"
                );
                assert!(!doc.flag(doc.field(root, "deadlock")));
                storm_assert_token(
                    &bench_doc,
                    &storm_group(doc.usz(doc.field(root, "monitor_checks"))),
                    path,
                );
            }
            "mixed" => {
                assert!(
                    doc.flag(doc.field(root, "deadlock")),
                    "{path}: the orphan leg must be named as a deadlock, not hidden in a rescue"
                );
                assert_eq!(deadlocks, reaped / lanes);
                assert_eq!(doc.usz(doc.field(root, "lease_kills")), 0);
            }
            _ => {
                assert_eq!(deadlocks, 0);
                assert_eq!(
                    ambush, reaped,
                    "{path}: every kill should land after the arm proof"
                );
                assert_eq!(doc.usz(doc.field(root, "lease_kills")), 0);
                assert_eq!(doc.usz(doc.field(root, "monitor_checks")), 0);
            }
        }

        let gens = generations
            .iter()
            .map(|g| doc.usz(*g).to_string())
            .collect::<Vec<String>>()
            .join(", ");
        let row = format!(
            "| {label} | {lanes} | {reaped} | {} | {ambush} / {escape} / {stillborn} | {deadlocks} | `conserved: true`, {breaks} breaks | {} ms |",
            storm_group(doc.usz(doc.field(root, "rescued"))),
            storm_group(doc.usz(doc.field(root, "wall_ms"))),
        );
        storm_assert_line(&bench_doc, &row, path, "pool");
        storm_assert_token(&bench_doc, &format!("[{gens}]"), path);
        storm_assert_token(&bench_doc, path, path);
        assert!(
            std::path::Path::new(path).exists(),
            "BENCHMARKS.md cites {path} and this oracle reads it, but the file is not \
             in the tree"
        );
    }

    // The guide describes the current tree, and a storm clock figure copied
    // into prose is stale on the next run with nothing to re-derive it: this
    // oracle rebuilds the storm numbers from the artifact, and BENCHMARKS.md
    // is the single home for them. The superseded 2026-10-26 set is the same
    // class of defect — a paragraph that names yesterday's artifact while the
    // evidence folder carries today's.
    let guide = read_doc("docs/USER_GUIDE.md");
    for token in ["wall_ms", "monitor_checks", "2026-10-26_pool_"] {
        assert!(
            !guide.contains(token),
            "docs/USER_GUIDE.md carries `{token}` — a wall-clock storm column that every \
             fresh run invalidates, or the superseded artifact set; state the deterministic \
             buckets and let BENCHMARKS.md carry the figures"
        );
    }
    assert!(
        guide.contains("2026-10-30_pool_"),
        "docs/USER_GUIDE.md does not name the committed pool artifacts its storm paragraph \
         describes — a guide that quotes a set no longer in docs/evidence reads as a claim \
         about nothing"
    );
}

/// Top-level keys of a report payload, read as the indentation the harness
/// writes: four spaces inside the PowerShell literal, eight inside the
/// Python dict. Nested payloads sit deeper, so their keys never enter.
fn storm_payload_keys(
    src: &str,
    marker: &str,
    stop: &str,
    key_of: fn(&str) -> Option<String>,
) -> Vec<String> {
    let at = src.find(marker).unwrap_or_else(|| {
        panic!(
            "harness lost the report payload `{marker}` — this parity tripwire is \
             the only thing that keeps one oracle able to read both artifacts"
        )
    });
    let mut out = Vec::new();
    for line in src[at + marker.len()..].split('\n').skip(1) {
        let bare = line.trim_end();
        if bare.starts_with(stop) {
            break;
        }
        if let Some(key) = key_of(bare) {
            out.push(key);
        }
    }
    out
}

fn ps_key(line: &str) -> Option<String> {
    let rest = line.strip_prefix("    ")?;
    if rest.starts_with(' ') || rest.starts_with('#') || rest.is_empty() {
        return None;
    }
    let name = rest.split_whitespace().next()?;
    rest[name.len()..]
        .trim_start()
        .starts_with('=')
        .then(|| name.to_string())
}

fn py_key(line: &str) -> Option<String> {
    let rest = line.strip_prefix("        \"")?;
    let name = rest.split("\":").next()?;
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

/// The two harnesses must publish one schema. An oracle that reads a Windows
/// report and a POSIX report by different keys is not one oracle, and the
/// first asymmetry to appear will be the one nobody notices.
#[test]
fn test_storm_harnesses_publish_one_report_schema() {
    let ps = read_doc("scripts/stress_multi_lineage.ps1");
    let py = read_doc("scripts/stress_multi_lineage.py");

    let ps_keys = storm_payload_keys(&ps, "$reportDoc = [ordered]@{", "}", ps_key);
    let py_keys = storm_payload_keys(&py, "report_doc = {", "    }", py_key);
    assert!(
        ps_keys.len() >= 25,
        "only {} PowerShell report keys parsed — the payload reflowed and this \
         tripwire now reads nothing",
        ps_keys.len()
    );
    assert!(
        py_keys.len() >= 25,
        "only {} Python report keys parsed — the payload reflowed and this \
         tripwire now reads nothing",
        py_keys.len()
    );
    let shared = ps_keys.len().min(py_keys.len());
    assert_eq!(
        ps_keys,
        &py_keys[..shared],
        "the two harnesses emit different top-level report keys; one oracle has \
         to read both artifacts"
    );
    assert_eq!(
        py_keys[shared..].join(","),
        "platform,kills_dispatched,fd_sampling",
        "Python-only report keys must stay exactly the POSIX extras, appended \
         after the shared set (got {:?})",
        &py_keys[shared..]
    );

    // Band keys are disjoint counts, not running totals. The rename away from
    // `le_*` is part of report_schema 2, so neither harness may drift back.
    for (name, src) in [("ps1", &ps), ("py", &py)] {
        assert!(
            src.contains("band_") && !src.contains("le_{") && !src.contains("\"le_"),
            "{name}: the histogram keys are no longer the disjoint `band_*` set"
        );
    }
}

/// Both harnesses must gate the shape they ran. `-ArmedHold` / `--armed-hold`
/// reaches the worker as a CLI flag, and the report carries what the worker
/// actually stamped; without that gate a lane that silently lost the flag
/// still publishes a PASS under the armed-hold label, which is the row both
/// BENCHMARKS.md and the CI annotations quote. One emission site per harness:
/// a second site would double-count against `lanes x per_lane + aggregate`.
#[test]
fn test_both_storm_harnesses_gate_the_armed_hold_shape() {
    let ps = read_doc("scripts/stress_multi_lineage.ps1");
    let py = read_doc("scripts/stress_multi_lineage.py");

    assert_eq!(
        ps.match_indices("\"$tag armed_hold matches flag\"").count(),
        1,
        "the PowerShell storm harness no longer gates armed_hold exactly once per lane"
    );
    assert_eq!(
        py.match_indices("f\"{tag} armed_hold matches\"").count(),
        1,
        "the POSIX storm harness no longer gates armed_hold exactly once per lane"
    );
}

/// Every `.ps1` in `scripts/` must be pure ASCII. The CI runner reads a
/// BOM-less UTF-8 file as cp1252, where an em dash's third byte (`0x94`)
/// decodes as U+201D and a minus sign's (`0x92`) as U+2019 — quote characters
/// to the PowerShell parser. Local `Parser::ParseFile` cannot see this because
/// the local machine's ACP is UTF-8, so the law needs a gate that reads bytes
/// rather than lines.
#[test]
fn test_powershell_scripts_are_pure_ascii() {
    let mut files: Vec<_> = std::fs::read_dir("scripts")
        .expect("read scripts dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("ps1"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 4,
        "only {} .ps1 scripts scanned — the sweep reads nothing if the layout moved",
        files.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    for path in &files {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let bad: Vec<(usize, u8)> = bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b > 0x7F)
            .map(|(i, b)| (i + 1, *b))
            .collect();
        if !bad.is_empty() {
            offenders.push(format!(
                "{}: {} non-ASCII byte(s), first at offset {} ({:02X})",
                path.display(),
                bad.len(),
                bad[0].0,
                bad[0].1
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "PowerShell scripts carry non-ASCII bytes, which the cp1252 runner decodes as \
         quote characters: {offenders:?}"
    );
}
