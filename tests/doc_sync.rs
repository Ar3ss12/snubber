//! Doc tripwires: the FLOOR MAP anchors in `src/node.rs` must never rot.
//!
//! Line numbers in comments die the day someone adds an import. Text tags
//! don't. If a refactor deletes or renames an anchor, this test goes red
//! and names the missing tag.

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

/// README test-count badge must mirror the declared totals in AGENTS.md.
///
/// Manual badge numbers rot: one added test without a badge bump = README
/// lies. This gate pins the badge to the AGENTS.md totals line so any
/// count move without a badge sync goes red in CI.
#[test]
fn test_readme_badge_mirrors_agent_totals() {
    let agents = std::fs::read_to_string("AGENTS.md").expect("read AGENTS.md");
    let readme = std::fs::read_to_string("README.md").expect("read README.md");
    // AGENTS.md §0 declares: `cargo test` today = **214 passed** ...
    // + 23 doc-tests = **237 green**.
    for pin in ["**214 passed**", "+ 23 doc-tests", "**237 green**"] {
        assert!(
            agents.contains(pin),
            "TRIPWIRE FIRED: AGENTS.md lost totals pin '{pin}' — update §0",
        );
    }
    // README badge must carry the same grand total.
    assert!(
        readme.contains("tests-237%20passed"),
        "TRIPWIRE FIRED: README badge drifted from AGENTS.md totals — \
         sync the tests badge with the §0 grand total",
    );
}
