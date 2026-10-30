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

/// The "not for" list named three things that are all about scope, none about
/// BLINDNESS, so a reader could assume a stability layer also watches the
/// desktop, the hooks and the buttons. It does not: the crate makes no OS call.
/// The README must keep the platform boundary page, name each blind spot, and
/// name the piece the crate DOES own, so a missing watchdog reads as a contract
/// rather than a bug.
#[test]
fn test_readme_states_the_platform_boundary() {
    let readme = read_doc("README.md");
    const HEADING: &str = "### Platform boundary";
    let start = readme.find(HEADING).expect(
        "TRIPWIRE FIRED: README.md lost the platform-boundary page — the \
                \"not for\" list names scope, not blindness (secure desktop, a \
                removed hook, a held button)",
    );
    let after = start + HEADING.len();
    let end = readme[after..]
        .find("\n## ")
        .map(|n| after + n)
        .unwrap_or(readme.len());
    let section = &readme[start..end];
    assert!(
        section.len() > 900,
        "TRIPWIRE FIRED: the platform-boundary section shrank to {} bytes — it \
         must name each blind spot, not gesture at one",
        section.len()
    );

    for token in [
        "secure desktop",
        "OpenInputDesktop",
        "SetWindowsHookEx",
        "SendInput",
        "MemoryTrend",
        "HandleWatch",
        "poll_rollback",
    ] {
        assert!(
            section.contains(token),
            "TRIPWIRE FIRED: the platform-boundary page no longer names `{token}` \
             — the crate detects none of these itself, and a reader must be told \
             which half is the app's"
        );
    }

    let when = readme
        .find("## When to use / when not")
        .expect("TRIPWIRE FIRED: README lost the use/not-for section");
    let when_end = readme[when..]
        .find("\n## ")
        .map(|n| when + n)
        .unwrap_or(readme.len());
    assert!(
        readme[when..when_end].contains("Not a platform monitor"),
        "TRIPWIRE FIRED: the \"not for\" list must point at the platform boundary \
         — a reader who stops at that list must still learn the crate is blind to \
         the desktop, the hooks and held buttons"
    );
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

/// The termination-storm tables in BENCHMARKS.md quote the committed
/// multi-lineage report JSONs in docs/evidence/. Every headline token is
/// derived from the artifact at test time — the docs never carry an
/// independent copy of a storm number. The JSONs are read with a tiny
/// key-number extractor (the crate has no serde in the dev graph); a
/// changed report shape must fail loudly here, not pass vacuously.
#[test]
fn test_storm_tables_match_committed_reports() {
    fn report_number(text: &str, key: &str) -> f64 {
        let needle = format!("\"{key}\":");
        let pos = text.find(&needle).unwrap_or_else(|| {
            panic!(
                "storm report lost key `{key}` — the harness report shape \
                 changed; refresh docs/evidence and this oracle together"
            )
        });
        let rest = text[pos + needle.len()..].trim_start();
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .unwrap_or(rest.len());
        rest[..end]
            .parse::<f64>()
            .unwrap_or_else(|_| panic!("storm report key `{key}` is not a plain number"))
    }

    fn dispatch_p50(text: &str) -> f64 {
        let pos = text
            .find("\"dispatch_us\":")
            .expect("storm report lost `dispatch_us`");
        report_number(&text[pos..], "p50")
    }

    fn thousands(n: u64) -> String {
        let digits = n.to_string();
        let mut out = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out
    }

    fn assert_in_doc(doc: &str, token: &str, artifact: &str) {
        assert!(
            doc.contains(token),
            "TRIPWIRE FIRED: BENCHMARKS.md storm table lost '{token}' but \
             {artifact} still carries it. Re-run the storm, refresh the \
             artifact, and update every storm citation to the artifact \
             value — never the artifact to the docs."
        );
    }

    let bench_doc = read_doc("BENCHMARKS.md");

    let armed = read_doc("docs/evidence/2026-10-27_multi_lineage_armedhold_report.json");
    let hold_pos = armed
        .find("\"armed_hold\":")
        .expect("armed-hold artifact lost the `armed_hold` key");
    let value = armed[hold_pos + "\"armed_hold\":".len()..]
        .trim_start()
        .chars()
        .take(4)
        .collect::<String>();
    assert!(
        value == "true",
        "armed-hold artifact lost `armed_hold: true` (found `{value}`)"
    );
    assert_in_doc(
        &bench_doc,
        &format!("{:.2} s", report_number(&armed, "duration_ms") / 1000.0),
        "armed-hold",
    );
    assert_in_doc(
        &bench_doc,
        &format!("{:.1}", report_number(&armed, "rate_per_sec")),
        "armed-hold",
    );
    assert_in_doc(
        &bench_doc,
        &format!("{:.1}", report_number(&armed, "peak_rate_per_sec")),
        "armed-hold",
    );
    assert_in_doc(
        &bench_doc,
        &format!("{} µs", dispatch_p50(&armed).round() as u64),
        "armed-hold",
    );
    assert_in_doc(
        &bench_doc,
        &format!(
            "{} ({:.0}%)",
            thousands(report_number(&armed, "true_ambush") as u64),
            report_number(&armed, "ambush_pct")
        ),
        "armed-hold",
    );

    let free = read_doc("docs/evidence/2026-10-27_multi_lineage_freerun_report.json");
    assert_in_doc(
        &bench_doc,
        &format!("{:.2} s", report_number(&free, "duration_ms") / 1000.0),
        "free-running",
    );
    assert_in_doc(
        &bench_doc,
        &format!("{:.1}", report_number(&free, "rate_per_sec")),
        "free-running",
    );
    assert_in_doc(
        &bench_doc,
        &format!("{:.1}", report_number(&free, "peak_rate_per_sec")),
        "free-running",
    );
    assert_in_doc(
        &bench_doc,
        &format!("{} µs", dispatch_p50(&free).round() as u64),
        "free-running",
    );
    let kills = report_number(&free, "kills_completed");
    let pct = |n: f64| n * 100.0 / kills;
    assert_in_doc(
        &bench_doc,
        &format!(
            "{} ({:.1}%)",
            thousands(report_number(&free, "true_ambush") as u64),
            pct(report_number(&free, "true_ambush"))
        ),
        "free-running",
    );
    assert_in_doc(
        &bench_doc,
        &format!(
            "{} ({:.1}%)",
            thousands(report_number(&free, "clean_escape") as u64),
            pct(report_number(&free, "clean_escape"))
        ),
        "free-running",
    );
    assert_in_doc(
        &bench_doc,
        &format!(
            "{} ({:.1}%)",
            thousands(report_number(&free, "stillborn") as u64),
            pct(report_number(&free, "stillborn"))
        ),
        "free-running",
    );

    for (name, text) in [("armed-hold", &armed), ("free-running", &free)] {
        let sum = report_number(text, "true_ambush")
            + report_number(text, "clean_escape")
            + report_number(text, "stillborn");
        assert_eq!(
            sum,
            report_number(text, "kills_completed"),
            "{name} artifact buckets do not conserve kills — the artifact \
             itself is broken, the docs cannot be checked against it"
        );
    }
}
