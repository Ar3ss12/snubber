//! Benchmark hygiene tripwire: bench setup must stay outside the hot loop.
//!
//! Law: creating `SnubberNode` / `IsrBridge` / `TaskConduit` inside
//! `b.iter(|| ...)` or `b.iter_custom(|iters| ...)` measures mutex
//! allocation and structure memory, not pure transactional overhead.
//! Setup belongs BEFORE the timing closure; inside only admission,
//! `black_box` payload, and commit/abort may run. This grep-gate keeps
//! every bench in `benches/admission_bench.rs` honest: heavy constructors
//! must appear textually before the timing call, never inside it.

use std::fs;

/// Read one source file or panic with the path.
fn src(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|_| panic!("missing source: {path}"))
}

/// Split one bench body out by brace matching from its name marker.
fn bench_body(source: &str, marker: &str) -> Option<String> {
    let start = source.find(marker)?;
    let open = source[start..].find('|')?;
    let (mut depth, mut begin, mut end) = (0usize, None, None);
    for (i, ch) in source[start + open..].char_indices() {
        if ch == '{' {
            if begin.is_none() {
                begin = Some(start + open + i);
            }
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                end = Some(start + open + i);
                break;
            }
        }
    }
    Some(source[begin?..=end?].to_string())
}

/// Split body into (setup, timed) at the first timing call.
fn split_at_timer(body: &str) -> (String, String) {
    let pos = match (
        body.find("b.iter("),
        body.find("b.iter_custom("),
        body.find("b.iter_batched("),
    ) {
        (Some(a), Some(b), Some(c)) => Some(a.min(b).min(c)),
        (Some(a), Some(b), None) => Some(a.min(b)),
        (Some(a), None, Some(c)) => Some(a.min(c)),
        (None, Some(b), Some(c)) => Some(b.min(c)),
        (Some(a), None, None) => Some(a),
        (None, Some(b), None) => Some(b),
        (None, None, Some(c)) => Some(c),
        (None, None, None) => None,
    };
    match pos {
        Some(p) => (body[..p].to_string(), body[p..].to_string()),
        None => (body.to_string(), String::new()),
    }
}

/// Heavy constructors: must live in setup, never in timed.
const HEAVY: &[&str] = &[
    "SnubberNode::builder(",
    "IsrBridge::new(",
    "TaskConduit::new(",
    "TaskConduit::pair(",
    "ConflateSender::<",
    "WorkerPool::new(",
    "PipeDrain::drain_sync(",
];

/// Constructors that ARE the measured API: `drain_sync` performs the
/// whole scan in its constructor, so per-sample construction is the
/// honest cost of `drain_sync_256_frames`. Every such site must be
/// declared here; any undeclared constructor is still a bug.
const PER_SAMPLE: &[(&str, &str)] = &[("drain_sync_256_frames", "PipeDrain::drain_sync(")];

/// Benches whose argument is BUILT per sample but must be built in the
/// `iter_batched` setup closure, never inside the timed routine. The
/// hygiene law here is weaker than text position: the drain is an
/// argument to `attach`, so `iter_batched` is the only way to time the
/// attach+reap pair without folding construction into it.
const BATCHED_SETUP: &[&str] = &["lane_attach_reap_ambush", "lane_attach_reap_escape"];

/// Fixed-name benches; `bench_function(format!(...))` sites are pinned by
/// their IDEAL markers below instead.
const COVERED: &[&str] = &[
    "guard_new_commit",
    "epoch_gate_observe",
    "classify_stall_ceiling",
    "worker_pulse_kick_watch",
    "isr_bridge_push_pop",
    "isr_trip_latch_swap",
    "node_enter_commit",
    "node_enter_auto_commit",
    "node_scoped_work_commit",
    "node_enter_critical_priority",
    "node_transition",
    "node_passthrough_enter_commit",
    "node_passthrough_scoped_commit",
    "gateway_roundtrip_send_recv",
    "gateway_send_throttled_latch",
    "conduit_check_directive",
    "node_next_sequence",
    "node_stats_snapshot",
    "stats_telemetry_math",
    "erased_enter_auto_commit",
    "conflate_publish_take",
    "conflate_publish_replace",
    "node_commit_enveloped_payload",
    "process_token_to_bytes",
    "process_token_parse",
    "state_digest_new_crc",
    "static_node_enter_commit",
    "snapshot_v1_encode_crc",
    "drain_sync_256_frames",
    "drain_feed_frame",
    "drain_feed_256_frames",
    "token_write_to",
    "state_digest_write_to",
    "namespace_task_pack",
    "namespace_task_unpack",
    "lane_attach_reap_ambush",
    "lane_attach_reap_escape",
    "report_snapshot_4_lanes",
    "note_memory_kb",
    "expired_lanes_4_lanes",
    "contended_node_enter_threads_",
    "contended_node_scoped_threads_",
    "contended_gateway_mpsc_threads_",
    "cross_thread_isr_bridge_push_pop",
    "friction_handler_arc_call",
    "friction_wait_adaptive_arc",
    "friction_wait_adaptive_fn",
    "node_name_clone_static",
    "node_name_clone_owned",
    "node_name_as_str",
    "async_enter_ready_poll",
    "async_park_wake_admit_roundtrip",
];

/// Split `b.iter_batched(setup, routine, size)` arguments at top-level
/// commas. Used to prove the timed routine does not construct anything.
fn iter_batched_args(timed: &str) -> Option<Vec<String>> {
    let marker = "b.iter_batched(";
    let start = timed.find(marker)? + marker.len();
    let bytes = timed.as_bytes();
    let mut depth = 0i32;
    let mut args = Vec::new();
    let mut begin = start;
    let mut i = start;
    while i < timed.len() {
        match bytes[i] as char {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                if depth == 0 {
                    args.push(timed[begin..i].to_string());
                    args.retain(|arg| !arg.trim().is_empty());
                    return Some(args);
                }
                depth -= 1;
            }
            ',' if depth == 0 => {
                let arg = &timed[begin..i];
                if !arg.trim().is_empty() {
                    args.push(arg.to_string());
                }
                begin = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Extract literal names from every `bench_function("name", ...)` site.
///
/// Deliberately stricter than a substring marker: a site whose name is a
/// helper indirection must abort the gate, not silently disappear from the
/// scan. The only allowed variable spelling is the hoisted
/// `let name = format!(...)` used by the sized contended benches.
fn static_bench_names(source: &str) -> Vec<String> {
    const MARKER: &str = "bench_function";
    let mut names = Vec::new();
    let mut cursor = 0usize;
    while let Some(pos) = source[cursor..].find(MARKER) {
        let abs = cursor + pos;
        let after = &source[abs + MARKER.len()..];
        let after = after.trim_start().strip_prefix('(').unwrap_or_else(|| {
            panic!("{MARKER} site without '(' — helper indirection hides benches")
        });
        let after = after.trim_start();
        if after.starts_with("name,") {
            let window = &source[abs.saturating_sub(600)..abs];
            assert!(
                window.contains("let name = format!("),
                "{MARKER}(name, ..) without a nearby `let name = format!(..)` binding"
            );
            cursor = abs + MARKER.len();
            continue;
        }
        let after = after.strip_prefix('"').unwrap_or_else(|| {
            panic!("{MARKER} with a non-literal name — hygiene gate cannot see it")
        });
        let end = after.find('"').expect("unterminated bench name");
        names.push(after[..end].to_string());
        cursor = abs + MARKER.len();
    }
    assert!(
        !names.is_empty(),
        "no {MARKER} sites found — the scan is blind"
    );
    names
}

#[test]
fn bench_setup_outside_hot_loop() {
    let source = src("benches/admission_bench.rs");
    let discovered = static_bench_names(&source);
    let mut unique = discovered.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), discovered.len(), "duplicate bench name");
    for name in &discovered {
        assert!(
            COVERED.contains(&name.as_str()),
            "bench '{name}' is not registered in COVERED"
        );
    }
    for name in COVERED {
        if name.ends_with('_') {
            continue;
        }
        assert!(
            discovered.iter().any(|n| n == name),
            "COVERED entry '{name}' has no bench in the source"
        );
    }
    // Sized contended benches build their name with format! and pass the
    // variable; each prefix must have its IDEAL pin.
    let sized = source.matches("g.bench_function(name,").count();
    assert_eq!(sized, 3, "sized contended bench count changed");
    assert_eq!(
        COVERED.iter().filter(|n| n.ends_with('_')).count(),
        sized,
        "every sized bench needs one prefix in COVERED"
    );
    for name in COVERED {
        // Prefix entries (contended_*_threads_) match sized format! benches:
        // verify their shared hoisted block, not the whole file.
        if name.ends_with('_') {
            let block_marker = match *name {
                "contended_node_enter_threads_" => "IDEAL-1",
                "contended_node_scoped_threads_" => "IDEAL-2",
                "contended_gateway_mpsc_threads_" => "IDEAL-3",
                _ => continue,
            };
            let pos = source.find(block_marker).expect("IDEAL block missing");
            // Bound the block at the NEXT ideal marker so IDEAL-1's timed
            // half cannot see IDEAL-2's hoisted setup (both contain builders).
            let rest = &source[pos..];
            let mut bound = rest.len();
            for m in ["IDEAL-1", "IDEAL-2", "IDEAL-3", "IDEAL-4"] {
                if m == block_marker {
                    continue;
                }
                if let Some(p) = rest[block_marker.len()..].find(m) {
                    bound = bound.min(block_marker.len() + p);
                }
            }
            // IDEAL-3's own gateway ctor lives in its timed half by design
            // (per-sample channel lifecycle): the slice check below applies
            // only to IDEAL-1/2 node builders.
            let block = &rest[..bound];
            let (_setup, timed) = split_at_timer(block);
            for ctor in HEAVY {
                // IDEAL-3 gateway is per-sample channel lifecycle (own queue
                // + drain thread per sample), not per-op alloc: allowed.
                if *name == "contended_gateway_mpsc_threads_" {
                    continue;
                }
                assert!(
                    !timed.contains(ctor),
                    "bench '{name}' builds '{ctor}' inside timer"
                );
            }
            continue;
        }
        let marker = format!("\"{name}\"");
        let body = bench_body(&source, &marker).unwrap_or_else(|| {
            if name.ends_with('_') {
                return source.clone();
            }
            panic!("bench '{name}' missing");
        });
        let (_setup, timed) = split_at_timer(&body);
        let has_timer = timed.contains("b.iter(")
            || timed.contains("b.iter_custom(")
            || timed.contains("b.iter_batched(");
        assert!(
            has_timer || name.ends_with('_'),
            "bench '{name}' has no timing call"
        );
        let name_s: &str = name;
        if BATCHED_SETUP.contains(&name_s) {
            // Construction must live in the setup closure. Proving that
            // `iter_batched` merely EXISTS is not enough: the routine
            // closure (second argument) is inside the timed window and must
            // stay constructor-free.
            assert!(
                timed.contains("b.iter_batched("),
                "bench '{name}' must time via iter_batched so drain construction is untimed"
            );
            let args = iter_batched_args(&timed)
                .unwrap_or_else(|| panic!("bench '{name}': cannot split iter_batched arguments"));
            assert_eq!(
                args.len(),
                3,
                "bench '{name}': iter_batched needs (setup, routine, size)"
            );
            for ctor in HEAVY {
                assert!(
                    !args[1].contains(ctor),
                    "bench '{name}' builds '{ctor}' inside the timed routine"
                );
            }
            assert!(
                HEAVY.iter().any(|ctor| args[0].contains(ctor)),
                "bench '{name}': batched setup builds no heavy constructor — stale BATCHED_SETUP entry"
            );
            continue;
        }
        for ctor in HEAVY {
            if PER_SAMPLE.iter().any(|(n, c)| *n == name_s && *c == *ctor) {
                continue;
            }
            assert!(
                !timed.contains(ctor),
                "bench '{name}' builds '{ctor}' inside timer"
            );
        }
    }
    // A discarding writer skips the copy the API is being measured on:
    // `sink()` understates every `write_to` bench. Reject the exact call
    // and the `use ...::sink [as alias]` spellings that hide it.
    for needle in ["std::io::sink", "io::sink", "sink(", "sink as", "{sink"] {
        assert!(
            !source.contains(needle),
            "benches must write into a real buffer, found '{needle}'"
        );
    }
    for (tag, pin) in [
        ("IDEAL-1", "bench_contended_enter"),
        ("IDEAL-2", "bench_contended_scoped"),
        ("IDEAL-4", "IsrBridge::new()"),
    ] {
        assert!(source.contains(pin), "{tag} lost pin '{pin}'");
    }
}

/// Every `[[bench]]` target in Cargo.toml must be the file this gate scans.
/// A second bench file would otherwise run unexamined (setup-in-timer etc.).
#[test]
fn every_registered_bench_target_is_hygiene_checked() {
    let manifest = src("Cargo.toml");
    let mut targets: Vec<String> = Vec::new();
    let mut in_bench = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line == "[[bench]]" {
            in_bench = true;
            continue;
        }
        if in_bench {
            if line.starts_with('[') {
                in_bench = false;
                continue;
            }
            if let Some(value) = line.strip_prefix("name") {
                let value = value.trim_start();
                if let Some(value) = value.strip_prefix('=') {
                    targets.push(value.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    assert!(
        !targets.is_empty(),
        "no [[bench]] target found in Cargo.toml — the hygiene gate would scan an unregistered file"
    );
    assert_eq!(
        targets,
        vec!["admission_bench".to_string()],
        "new bench target(s) {targets:?}: extend this gate to scan them before registering"
    );
    assert!(
        std::path::Path::new("benches/admission_bench.rs").exists(),
        "registered bench file is missing"
    );
}
