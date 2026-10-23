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
    cargo bench --bench admission_bench
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
else
    echo -e "\033[31mError: neither python3 nor python found in PATH.\033[0m"
    exit 1
fi

# Python or jq/awk parser for Criterion estimates.json
"$PY" - << 'EOF' > "${OUT_FILE}"
import os
import json
import glob

criterion_dir = os.path.join("target", "criterion")

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
    "gateway_roundtrip_send_recv": "gateway send -> recv roundtrip",
    "gateway_send_throttled_latch": "gateway send (HWM/LWM throttled)",
    "conduit_check_directive": "TaskConduit::check_directive",
    "node_next_sequence": "node.next_sequence (auto ticket)",
    "node_stats_snapshot": "node.stats() snapshot",
    "stats_telemetry_math": "SnubberStats::shed_total + success_permille",
    "erased_enter_auto_commit": "enter_erased_auto",
}

print("| Operation | Average Latency | Notes |")
print("|---|---|---|")

pattern = os.path.join(criterion_dir, "**", "estimates.json")
estimate_files = sorted(glob.glob(pattern, recursive=True))

for path in estimate_files:
    norm = os.path.normpath(path).replace("\\", "/")
    parts = norm.split("/")
    
    # Filter only primary runs: <group>/<function>/estimates.json
    # Ignore new/base subdirectories
    if len(parts) >= 3 and parts[-2] not in ("new", "base"):
        key = parts[-2]
        group = parts[-3] if len(parts) >= 4 else "benchmark"
        label = labels.get(key, key)
        
        try:
            with open(path, "r", encoding="utf-8") as f:
                data = json.load(f)
                mean_ns = float(data["mean"]["point_estimate"])
                
                if mean_ns < 1000.0:
                    latency = f"{mean_ns:.1f} ns"
                else:
                    latency = f"{mean_ns / 1000.0:.2f} µs"
                    
                print(f"| `{label}` | **{latency}** | {group} bench |")
        except Exception:
            continue
EOF

echo ""
echo -e "\033[32m=== BENCH SNAPSHOT (ns/op mean point estimate) ===\033[0m"
cat "${OUT_FILE}"
echo -e "\033[36mSnapshot written: ${OUT_FILE}\033[0m"

if [ "$GITHUB_MODE" = true ] && [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    cat "${OUT_FILE}" >> "${GITHUB_STEP_SUMMARY}"
fi
