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
    let pos = match (body.find("b.iter("), body.find("b.iter_custom(")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
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
/// whole scan in its constructor, and `lane.attach` takes ownership of a
/// drain, so per-sample construction is the honest cost there. Every such
/// site must be declared here; any undeclared constructor is still a bug.
const PER_SAMPLE: &[(&str, &str)] = &[
    ("drain_sync_256_frames", "PipeDrain::drain_sync("),
    ("lane_attach_reap_ambush", "PipeDrain::drain_sync("),
    ("lane_attach_reap_escape", "PipeDrain::drain_sync("),
];

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

/// Extract literal names from `g.bench_function("name"` sites.
fn static_bench_names(source: &str) -> Vec<String> {
    const MARKER: &str = "g.bench_function(\"";
    let mut names = Vec::new();
    let mut rest = source;
    while let Some(pos) = rest.find(MARKER) {
        let after = &rest[pos + MARKER.len()..];
        match after.find('"') {
            Some(end) => names.push(after[..end].to_string()),
            None => break,
        }
        rest = after;
    }
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
        assert!(
            timed.contains("b.iter(") || timed.contains("b.iter_custom(") || name.ends_with('_')
        );
        let name_s: &str = name;
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
    for (tag, pin) in [
        ("IDEAL-1", "bench_contended_enter"),
        ("IDEAL-2", "bench_contended_scoped"),
        ("IDEAL-4", "IsrBridge::new()"),
    ] {
        assert!(source.contains(pin), "{tag} lost pin '{pin}'");
    }
}
