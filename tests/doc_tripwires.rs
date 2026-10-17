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

/// Every low-level entry must carry the scare-strip pointing at the
/// canonical worker entry. A newcomer hovering over `enter` in an IDE must
/// see "prefer enter_erased_auto" before touching the hydraulics.
#[test]
fn test_low_level_entries_point_at_canonical() {
    let source =
        std::fs::read_to_string("src/node.rs").expect("Failed to read src/node.rs for entry audit");

    // One scare-strip per low-level method. `enter_erased_auto` is the
    // canonical target and must NOT carry the strip itself.
    let strip = "prefer the canonical [`Self::enter_erased_auto`]";
    let strips = source.matches(strip).count();
    assert_eq!(
        strips, 8,
        "TRIPWIRE FIRED: want exactly 8 scare-strips (enter, \
         enter_with_priority, enter_with_generation, enter_erased, enter_auto, enter_checked_auto, \
         enter_checked, enter_supervised_erased), found {strips}.",
    );

    // The canonical entry advertises itself with the green header.
    assert!(
        source.contains("# 🟢 CANONICAL APPLICATION ENTRY"),
        "TRIPWIRE FIRED: canonical header missing from enter_erased_auto.",
    );
}
