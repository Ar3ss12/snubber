#!/usr/bin/env bash
# Bench threshold gate for the scheduled bench-full lane: fails on a ~10x
# regression of a key operation. Ceilings are deliberately coarse (~10x the
# 2026-10-27 local Criterion means) so runner noise cannot flake the lane;
# precision stays with the README benchmark table. A missing estimates.json
# is a FAIL, not a SKIP -- an unexecuted bench proves nothing.
#
# Usage: bash scripts/bench_gate.sh [criterion-root]
set -euo pipefail

ROOT="${1:-target/criterion}"

# group/bench:ceiling_ns (mean point estimate, nanoseconds)
KEYS=(
  layer1_core/guard_new_commit:100
  layer1_core/epoch_gate_observe:100
  layer2_std/node_enter_commit:1500
  layer2_std/node_enter_auto_commit:1500
  layer2_std/node_enter_critical_priority:1500
  layer2_std/node_passthrough_enter_commit:1000
  layer2_std/node_passthrough_scoped_commit:1000
  layer2_std/node_scoped_work_commit:1500
  layer2_std/node_transition:1500
  layer3_alloc/erased_enter_auto_commit:3000
  layer2_async_waiters/async_enter_ready_poll:2500
  layer2_async_waiters/async_park_wake_admit_roundtrip:8000
  layer1_5_hang_isr/isr_bridge_push_pop:150
  layer2_impedance/conflate_publish_take:500
  layer4_drain_feed/drain_feed_frame:1500
  layer4_layer5_diagnostics/process_token_to_bytes:150
  layer4_worker_pool/report_snapshot_4_lanes:1500
)

if [ ! -d "$ROOT" ]; then
  echo "FAIL: criterion root '$ROOT' does not exist (run cargo bench first)"
  exit 1
fi

fail=0
for key in "${KEYS[@]}"; do
  path="${key%%:*}"
  ceiling="${key##*:}"
  est="$ROOT/$path/new/estimates.json"
  if [ ! -f "$est" ]; then
    echo "FAIL: missing $est (bench did not run)"
    fail=1
    continue
  fi
  mean=$(python3 - "$est" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["mean"]["point_estimate"])
PY
)
  ok=$(awk -v m="$mean" -v c="$ceiling" 'BEGIN{print (m<=c)?1:0}')
  if [ "$ok" != 1 ]; then
    printf 'FAIL: %s mean %.0f ns exceeds ceiling %s ns\n' "$path" "$mean" "$ceiling"
    fail=1
  else
    printf 'OK: %s mean %.0f ns <= %s ns\n' "$path" "$mean" "$ceiling"
  fi
done

if [ "$fail" != 0 ]; then
  echo "bench_gate: FAIL"
else
  echo "bench_gate: all ${#KEYS[@]} ceilings hold"
fi
exit "$fail"
