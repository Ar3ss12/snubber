#!/usr/bin/env bash
# SNUBBER BENCH HARNESS (POSIX / Linux / macOS)
# Run Criterion benchmarks and export a float-free telemetry snapshot
# (ns/op, mean point estimate) into target/bench/snapshot.md.
#
# Usage:
#   ./scripts/bench.sh              # build + run + export
#   ./scripts/bench.sh --skip-run   # export from existing run
#   ./scripts/bench.sh --github     # CI: fresh run, step summary output

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# All relative paths (target/criterion, snapshot output) resolve from the
# repo root — including the --skip-run export path.
cd "${ROOT_DIR}"

BENCH_DIR="${ROOT_DIR}/target"
CRITERION_DIR="${BENCH_DIR}/criterion"
OUT_DIR="${BENCH_DIR}/bench"
OUT_FILE="${OUT_DIR}/snapshot.md"

SKIP_RUN=false
GITHUB_MODE=false

# CI detection is strict on purpose: an unrecognised value must behave like
# a local run, never like CI (and vice versa) -- see check_compile_gates.sh.
in_ci() {
    case "$(printf '%s' "${CI:-}" | tr '[:upper:]' '[:lower:]')" in
        true | 1) return 0 ;;
        *) return 1 ;;
    esac
}

for arg in "$@"; do
    case "$arg" in
        --skip-run|-s)
            SKIP_RUN=true
            ;;
        --github|-g)
            GITHUB_MODE=true
            ;;
        *)
            echo "Unknown argument: $arg"
            echo "Usage: $0 [--skip-run] [--github]"
            exit 1
            ;;
    esac
done

if [ "$SKIP_RUN" = false ]; then
    echo -e "\033[36m[bench] cargo bench --bench admission_bench (release) ...\033[0m"
    cd "${ROOT_DIR}"
    cargo bench --locked --bench admission_bench
fi

if [ ! -d "${CRITERION_DIR}" ]; then
    echo -e "\033[31mError: Criterion output not found at ${CRITERION_DIR}. Run without --skip-run first.\033[0m"
    exit 1
fi

mkdir -p "${OUT_DIR}"

# Python parser (python3 on POSIX; the Windows launcher installs as `python`)
if command -v python3 > /dev/null 2>&1; then
    PY=python3
elif command -v python > /dev/null 2>&1; then
    PY=python
    # The f-string parser below is Python 3 only: bare `python` may be a
    # Python 2 interpreter that dies with a SyntaxError mid-pipeline.
    if ! "$PY" -c 'import sys; sys.exit(0 if sys.version_info[0] >= 3 else 1)' 2> /dev/null; then
        echo -e "\033[31mError: 'python' is Python 2 — the parser needs Python 3 (install python3).\033[0m" >&2
        exit 1
    fi
else
    echo -e "\033[31mError: neither python3 nor python found in PATH.\033[0m"
    exit 1
fi

# Atomic publish: parse into a sibling temp file, validate, then move.
# A crashed parser must never leave a truncated or empty snapshot at
# OUT_FILE (the README table is generated from it).
TMP_OUT="$(mktemp "${OUT_DIR}/snapshot.XXXXXX")"
trap 'rm -f "${TMP_OUT}"' EXIT

"$PY" - << 'EOF' > "${TMP_OUT}"
import os
import sys
import json
import glob

criterion_dir = os.path.join("target", "criterion")

# Every bench in the suite is mapped: an unmapped key still exports, but
# under its snake_case function name, which reads as a different operation
# next to the mapped rows.
labels = {
    "guard_new_commit": "SnubberGuard::new + commit",
    "epoch_gate_observe": "EpochGate::observe",
    "classify_stall_ceiling": "classify_stall_ceiling",
    "worker_pulse_kick_watch": "WorkerPulse::kick + PulseWatch::observe",
    "isr_bridge_push_pop": "IsrBridge::try_push + try_pop",
    "isr_trip_latch_swap": "IsrBridge::trip + take_trips",
    "node_enter_commit": "node.enter + commit",
    "node_enter_auto_commit": "node.enter_auto + commit",
    "node_scoped_work_commit": "node.scoped (work + auto-commit)",
    "node_enter_critical_priority": "node.enter_with_priority(Critical)",
    "node_transition": "node.transition",
    "node_passthrough_enter_commit": "node_passthrough.enter + commit",
    "node_passthrough_scoped_commit": "node_passthrough.scoped + commit",
    "conflate_publish_take": "ConflateSender::publish + take_latest",
    "conflate_publish_replace": "ConflateSender::publish (occupied slot)",
    "node_commit_enveloped_payload": "guard.commit_enveloped(64 B)",
    "async_enter_ready_poll": "enter_async_auto (ready poll)",
    "async_park_wake_admit_roundtrip": "enter_async_auto park/wake/admit",
    "gateway_roundtrip_send_recv": "gateway send -> recv roundtrip",
    "gateway_send_throttled_latch": "gateway send (HWM/LWM throttled)",
    "conduit_check_directive": "TaskConduit::check_directive",
    "node_next_sequence": "node.next_sequence (auto ticket)",
    "node_stats_snapshot": "node.stats() snapshot",
    "stats_telemetry_math": "SnubberStats::shed_total + success_permille",
    "erased_enter_auto_commit": "enter_erased_auto",
    "drain_feed_frame": "PipeDrain::feed (one frame)",
    "drain_feed_256_frames": "PipeDrain::feed (256 frames)",
    "drain_sync_256_frames": "PipeDrain::drain_sync (256 frames)",
    "token_write_to": "ProcessToken::write_to(&mut sink)",
    "state_digest_write_to": "StateDigest::write_to(&mut sink)",
    "friction_handler_arc_call": "friction_fn handler call (Arc<dyn Fn>)",
    "friction_wait_adaptive_arc": "friction_wait_ms (adaptive, Arc handler)",
    "friction_wait_adaptive_fn": "friction_wait_ms (adaptive, fn)",
    "node_name_as_str": "NodeName::as_str",
    "node_name_clone_static": "NodeName::Static clone",
    "node_name_clone_owned": "NodeName::Owned clone",
    "process_token_parse": "ProcessToken::from_bytes",
    "process_token_to_bytes": "process_token.to_bytes()",
    "snapshot_v1_encode_crc": "snapshot_v1.encode()",
    "state_digest_new_crc": "state_digest.crc()",
    "static_node_enter_commit": "static_node.enter + commit",
    "expired_lanes_4_lanes": "pool.expired_lanes(lease) (4 lanes)",
    "lane_attach_reap_ambush": "lane.attach + note_death (ambush) -> PoolReap",
    "lane_attach_reap_escape": "lane.attach + note_death (escape) -> PoolReap",
    "namespace_task_pack": "namespace_task::pack",
    "namespace_task_unpack": "namespace_task::unpack",
    "note_memory_kb": "lane.note_memory_kb",
    "report_snapshot_4_lanes": "pool.report() (4 lanes)",
    "contended_node_enter_threads_6": "node.enter + commit (6 threads)",
    "contended_node_scoped_threads_6": "node.scoped (6 threads)",
    "contended_gateway_mpsc_threads_6": "gateway send (6 threads)",
    "cross_thread_isr_bridge_push_pop": "IsrBridge push/pop (2 threads)",
}

print("| Operation | Average Latency | Notes |")
print("|---|---|---|")

pattern = os.path.join(criterion_dir, "**", "estimates.json")
estimate_files = sorted(glob.glob(pattern, recursive=True))

for path in estimate_files:
    norm = os.path.normpath(path).replace("\\", "/")
    parts = norm.split("/")

    # Criterion 0.5 layout: <group>/<function>/new/estimates.json holds the
    # latency in ns; change/ holds a relative delta and base/ the old run.
    # Accept ONLY `new/` -- reading `change/` published 0.1 "ns" tables.
    if len(parts) < 5 or parts[-2] != "new":
        continue
    key = parts[-3]
    group = parts[-4]
    label = labels.get(key, key)

    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
            mean_ns = float(data["mean"]["point_estimate"])
            if not (mean_ns >= 0.0):
                raise ValueError(f"non-finite mean {mean_ns!r}")

            if mean_ns < 1000.0:
                latency = f"{mean_ns:.1f} ns"
            else:
                latency = f"{mean_ns / 1000.0:.2f} us"

            print(f"| `{label}` | **{latency}** | {group} bench |")
    except Exception as exc:
        # A corrupt or half-written estimate must fail the export: the
        # old `continue` silently dropped rows and still published.
        print(f"[bench] FAIL: cannot parse {path}: {exc}", file=sys.stderr)
        sys.exit(1)
EOF

if [ ! -s "${TMP_OUT}" ]; then
    # Header-only snapshot means the export saw no estimates: publishing
    # it would zero out the README table with a green run. Fail in every
    # mode (parity with bench.ps1), not just CI.
    echo -e "\033[31m[bench] FAIL: export produced an empty snapshot under ${CRITERION_DIR} — refusing to write it (run without --skip-run or check target/criterion)\033[0m" >&2
    exit 1
fi

# `grep -c` is the gate's only evidence that rows exist; its exit status
# was discarded, so a grep error produced an empty string that compared
# unequal to "0" and let an empty snapshot through.
ROWS_RC=0
ROWS="$(grep -c '^| `' "${TMP_OUT}")" || ROWS_RC=$?
if [ "${ROWS_RC}" -gt 1 ]; then
    echo -e "\033[31m[bench] FAIL: cannot scan ${TMP_OUT} for rows (grep exit ${ROWS_RC})\033[0m" >&2
    exit 1
fi
if [ "${ROWS:-0}" -eq 0 ]; then
    echo -e "\033[31m[bench] FAIL: no bench estimates found under ${CRITERION_DIR} — refusing to write an empty snapshot (run without --skip-run or check target/criterion)\033[0m" >&2
    exit 1
fi

mv -f "${TMP_OUT}" "${OUT_FILE}"
trap - EXIT

echo ""
echo -e "\033[32m=== BENCH SNAPSHOT (ns/op mean point estimate) ===\033[0m"
cat "${OUT_FILE}"
echo -e "\033[36mSnapshot written: ${OUT_FILE}\033[0m"

if [ "$GITHUB_MODE" = true ]; then
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        cat "${OUT_FILE}" >> "${GITHUB_STEP_SUMMARY}"
    elif in_ci; then
        # In CI the whole point of --github is the published summary; a
        # silent no-op reported a green bench job that showed nothing.
        echo -e "\033[31m[bench] FAIL: --github in CI but GITHUB_STEP_SUMMARY is unset — the snapshot would never reach the step summary\033[0m" >&2
        exit 1
    else
        echo -e "\033[33m[bench] WARN: --github without GITHUB_STEP_SUMMARY (not CI) — snapshot left at ${OUT_FILE}\033[0m" >&2
    fi
fi
