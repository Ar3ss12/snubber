//! Doc tripwires: the FLOOR MAP anchors in `src/node.rs` must never rot,
//! the L1–L5 layer map must keep its headings, order and canonical
//! one-line tagline across the three public docs, and every module path
//! quoted in prose must exist on disk.
//!
//! Line numbers in comments die the day someone adds an import. Text tags
//! don't. If a refactor deletes or renames an anchor, this test goes red
//! and names the missing tag.
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
/// see "prefer enter_erased_auto" before invoking low-level primitives.
#[test]
fn test_low_level_entries_point_at_canonical() {
    let source =
        std::fs::read_to_string("src/node.rs").expect("Failed to read src/node.rs for entry audit");

    const LOW_LEVEL_METHODS: &[&str] = &[
        "pub fn enter<F: FnOnce()>(",
        "pub fn enter_with_priority<F: FnOnce()>(",
        "pub fn enter_with_generation<F: FnOnce()>(",
        "pub fn enter_erased<F: FnOnce() + Send + 'static>(",
        "pub fn enter_auto<F: FnOnce()>(",
        "pub fn enter_checked_auto<P, V, F: FnOnce()>(",
        "pub fn enter_checked<P, V, F: FnOnce()>(",
        "pub fn enter_supervised_erased<F>(",
    ];

    let warning = "prefer the canonical [`Self::enter_erased_auto`]";

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

    // `enter_erased_auto` is the canonical target and must NOT carry the redirection itself.
    let canonical_pos = source
        .find("pub fn enter_erased_auto<")
        .expect("Canonical entry missing");
    let canonical_doc = &source[canonical_pos.saturating_sub(600)..canonical_pos];

    assert!(
        !canonical_doc.contains(warning),
        "Canonical entry must not carry a redirection warning to itself"
    );

    // The canonical entry advertises itself with the canonical header.
    assert!(
        source.contains("# 🟢 CANONICAL APPLICATION ENTRY"),
        "TRIPWIRE FIRED: canonical header missing from enter_erased_auto.",
    );
}

/// The erased-guard bulkhead boundary must stay at preset level in the
/// README: the erased path releases its slot at admission, so
/// `max_in_flight` bounds admissions, not live erased transactions.
#[test]
fn test_readme_states_erased_bulkhead_scope() {
    let readme = std::fs::read_to_string("README.md")
        .expect("Failed to read README.md for bulkhead-scope audit");
    assert!(
        readme.contains("**Bulkhead Scope (erased guards):**"),
        "TRIPWIRE FIRED: README lost the 'Bulkhead Scope (erased guards)' \
         paragraph — the erased-guard slot boundary must stay documented."
    );
    assert!(
        readme.contains("bounds concurrent *admissions*, not live erased transactions"),
        "TRIPWIRE FIRED: README bulkhead-scope paragraph lost its boundary \
         statement."
    );
    let source = std::fs::read_to_string("src/node.rs")
        .expect("Failed to read src/node.rs for bulkhead-scope audit");
    let pos = source
        .find("pub fn enter_erased_auto<")
        .expect("Canonical entry missing");
    let doc_window = &source[pos.saturating_sub(1200)..pos];
    assert!(
        doc_window.contains("Bulkhead scope"),
        "TRIPWIRE FIRED: enter_erased_auto doc lost the bulkhead-scope note."
    );
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
