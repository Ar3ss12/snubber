#!/usr/bin/env bash
# snubber process termination stress harness (Linux / macOS).
# Positional args (backward compatible): [target] [kills] [min_sleep_sec]
# Flags: --launch (start supervisor too), --build, --debug, --aggressive
set -u

TARGET_PROCESS="process_termination_loop"
KILLS=10000
MIN_SLEEP=0
LAUNCH=0
BUILD=0
AGGRESSIVE=0
PROFILE="release"
REPORT="target/termination_report.json"
STATS="target/stress_stats.json"
SAMPLE_EVERY=5
MAX_GROWTH_MB=2.0
POS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --launch) LAUNCH=1 ;;
        --build) BUILD=1 ;;
        --debug) PROFILE="debug" ;;
        --aggressive) AGGRESSIVE=1 ;;
        --max-growth-mb)
            shift
            case "${1:-}" in ''|*[!0-9.]*) echo "Error: --max-growth-mb requires a numeric value" >&2; exit 1 ;; esac
            MAX_GROWTH_MB="$1"
            ;;
        *) POS+=("$1") ;;
    esac
    shift
done
TARGET_PROCESS="${POS[0]:-$TARGET_PROCESS}"
KILLS="${POS[1]:-$KILLS}"
MIN_SLEEP="${POS[2]:-$MIN_SLEEP}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

PID_FILES=("target/supervised_worker.pid" "target/chaos_worker.pid")
EXE="target/$PROFILE/examples/$TARGET_PROCESS"
LAT_FILE="target/stress_latencies.tmp"
: > "$LAT_FILE"

HAVE_NS=0
if [ -n "$(date +%s%N 2>/dev/null | tr -dc '0-9')" ] && [ "$(date +%s%N 2>/dev/null | tr -dc '0-9' | wc -c)" -ge 18 ]; then
    HAVE_NS=1
fi

now_ms() {
    if [ "$HAVE_NS" = 1 ]; then echo $(( $(date +%s%N) / 1000000 )); else echo $(( SECONDS * 1000 )); fi
}

rss_kb() { awk '/^VmRSS:/ {print $2; exit}' "/proc/$1/status" 2>/dev/null; }
threads_n() { awk '/^Threads:/ {print $2; exit}' "/proc/$1/status" 2>/dev/null; }
fds_n() { ls "/proc/$1/fd" 2>/dev/null | wc -l | tr -d ' '; }
cpu_ticks() { sed 's/^[^)]*) //' "/proc/$1/stat" 2>/dev/null | awk '{print $12+$13}'; }

SUP_PID=""
SUP_EXIT=""
SUP_RSS_PEAK=0
SUP_RSS_SUM=0
SUP_RSS_N=0
SUP_RSS_FIRST=0
SUP_RSS_LAST=0
SUP_THREADS_PEAK=0
SUP_FDS_PEAK=0
SUP_CPU_TICKS=0
W_RSS_PEAK=0
W_RSS_SUM=0
W_RSS_N=0
W_THREADS_PEAK=0
W_THREADS_SUM=0
W_FDS_PEAK=0
W_FDS_SUM=0
PEAK_RATE=0
WIN_KILLS=0
WIN_T=0
CLK_TCK=$(getconf CLK_TCK 2>/dev/null || echo 100)

sample_sup() {
    [ -n "$SUP_PID" ] || return 0
    local rss th fds tks
    rss=$(rss_kb "$SUP_PID"); [ -n "$rss" ] || return 0
    th=$(threads_n "$SUP_PID")
    fds=$(fds_n "$SUP_PID")
    tks=$(cpu_ticks "$SUP_PID")
    [ "$SUP_RSS_N" -eq 0 ] && SUP_RSS_FIRST=$rss
    SUP_RSS_LAST=$rss
    SUP_RSS_SUM=$((SUP_RSS_SUM + rss)); SUP_RSS_N=$((SUP_RSS_N + 1))
    [ "$rss" -gt "$SUP_RSS_PEAK" ] && SUP_RSS_PEAK=$rss
    [ "${th:-0}" -gt "$SUP_THREADS_PEAK" ] && SUP_THREADS_PEAK=$th
    [ "${fds:-0}" -gt "$SUP_FDS_PEAK" ] && SUP_FDS_PEAK=$fds
    [ -n "${tks:-}" ] && SUP_CPU_TICKS=$tks
}

sample_worker() {
    local pid="$1" rss th fds
    rss=$(rss_kb "$pid"); [ -n "$rss" ] || return 0
    th=$(threads_n "$pid")
    fds=$(fds_n "$pid")
    W_RSS_SUM=$((W_RSS_SUM + rss)); W_RSS_N=$((W_RSS_N + 1))
    [ "$rss" -gt "$W_RSS_PEAK" ] && W_RSS_PEAK=$rss
    W_THREADS_SUM=$((W_THREADS_SUM + ${th:-0}))
    [ "${th:-0}" -gt "$W_THREADS_PEAK" ] && W_THREADS_PEAK=$th
    W_FDS_SUM=$((W_FDS_SUM + ${fds:-0}))
    [ "${fds:-0}" -gt "$W_FDS_PEAK" ] && W_FDS_PEAK=$fds
}

echo "=========================================================="
echo "   SNUBBER STRESS HARNESS: FORCED PROCESS TERMINATION     "
echo "=========================================================="
echo "Target   : $TARGET_PROCESS ($KILLS kills requested)"
echo "Signal   : kill -9 (SIGKILL)"
echo "Mode     : $([ "$LAUNCH" = 1 ] && echo "launch supervisor + kill its workers" || echo "external supervisor (kill only)")"
echo "Workload : $([ "$AGGRESSIVE" = 1 ] && echo "spin pacing (precise 3 ms armed / 1 ms pace)" || echo "sleep pacing (default)")"
echo "Profile  : $PROFILE"
echo "----------------------------------------------------------"

if [ "$LAUNCH" = 1 ]; then
    if [ "$BUILD" = 1 ] || [ ! -x "$EXE" ]; then
        echo "[BUILD] cargo build --example $TARGET_PROCESS ($PROFILE)..."
        if [ "$PROFILE" = "release" ]; then
            cargo build --release --example "$TARGET_PROCESS" || { echo "[FAIL] cargo build"; exit 1; }
        else
            cargo build --example "$TARGET_PROCESS" || { echo "[FAIL] cargo build"; exit 1; }
        fi
    fi
    [ -x "$EXE" ] || { echo "[FAIL] supervisor binary missing: $EXE (use --build)"; exit 1; }
    rm -f "${PID_FILES[0]}" "${PID_FILES[1]}" "$REPORT"
    SUP_ARGS=(--target-kills "$KILLS" --report-path "$REPORT")
    [ "$AGGRESSIVE" = 1 ] && SUP_ARGS+=(--spin)
    "./$EXE" "${SUP_ARGS[@]}" > target/stress_supervisor.out 2> target/stress_supervisor.err &
    SUP_PID=$!
    echo "[LAUNCH] supervisor pid=$SUP_PID"
    for _ in $(seq 1 200); do
        [ -f "${PID_FILES[0]}" ] && break
        kill -0 "$SUP_PID" 2>/dev/null || break
        sleep 0.05
    done
    if ! kill -0 "$SUP_PID" 2>/dev/null && [ ! -f "$REPORT" ]; then
        echo "[FAIL] supervisor exited before publishing a worker pid"
        tail -5 target/stress_supervisor.err 2>/dev/null
        exit 1
    fi
fi

EXECUTED=0
MISSES=0
NOT_FOUND=0
LAST_KILLED_PID=0
LAST_SUCCESS_MS=$(now_ms)
RUN_START_MS=$(now_ms)
WIN_T=$RUN_START_MS

while [ "$EXECUTED" -lt "$KILLS" ]; do
    TARGET_PID=0

    for f in "${PID_FILES[@]}"; do
        [ -f "$f" ] || continue
        PARSED=$(tr -d '[:space:]' < "$f" 2>/dev/null)
        case "$PARSED" in ''|*[!0-9]*) continue ;; esac
        [ "$PARSED" -eq "$LAST_KILLED_PID" ] && continue
        [ -n "$SUP_PID" ] && [ "$PARSED" -eq "$SUP_PID" ] && continue
        [ "$PARSED" -eq "$$" ] && continue
        if kill -0 "$PARSED" 2>/dev/null; then TARGET_PID="$PARSED"; break; fi
    done

    if [ "$TARGET_PID" -eq 0 ]; then
        CANDIDATE=$(pgrep -n -f "$TARGET_PROCESS" 2>/dev/null)
        if [ -n "$CANDIDATE" ] && [ "$CANDIDATE" -ne "$LAST_KILLED_PID" ] \
            && [ "$CANDIDATE" -ne "$$" ] && { [ -z "$SUP_PID" ] || [ "$CANDIDATE" -ne "$SUP_PID" ]; }; then
            TARGET_PID="$CANDIDATE"
        fi
    fi

    if [ "$TARGET_PID" -ne 0 ]; then
        START_NS=""
        [ "$HAVE_NS" = 1 ] && START_NS=$(date +%s%N)
        sample_worker "$TARGET_PID"
        if kill -9 "$TARGET_PID" 2>/dev/null; then
            if [ "$HAVE_NS" = 1 ]; then
                END_NS=$(date +%s%N)
                awk -v a="$START_NS" -v b="$END_NS" 'BEGIN { printf "%.3f\n", (b-a)/1000000 }' >> "$LAT_FILE"
            fi
            EXECUTED=$((EXECUTED + 1))
            LAST_KILLED_PID="$TARGET_PID"
            LAST_SUCCESS_MS=$(now_ms)

            if [ $((EXECUTED % 500)) -eq 0 ]; then
                NOW_MS=$(now_ms)
                WIN_RATE=$(awk -v n=$((EXECUTED - WIN_KILLS)) -v d=$((NOW_MS - WIN_T)) 'BEGIN { printf "%.1f", d > 0 ? 1000*n/d : 0 }')
                PEAK_RATE=$(awk -v a="$PEAK_RATE" -v b="$WIN_RATE" 'BEGIN { print (b > a) ? b : a }')
                WIN_KILLS=$EXECUTED
                WIN_T=$NOW_MS
            fi

            if [ $((EXECUTED % $SAMPLE_EVERY)) -eq 0 ]; then sample_sup; fi

            if [ $((EXECUTED % 500)) -eq 0 ] || [ "$EXECUTED" -le 10 ] || [ "$EXECUTED" -eq "$KILLS" ]; then
                NOW_MS=$(now_ms)
                RATE=$(awk -v n="$EXECUTED" -v d=$((NOW_MS - RUN_START_MS)) 'BEGIN { printf "%.1f", d > 0 ? 1000*n/d : n }')
                echo "[HARNESS] $EXECUTED/$KILLS | pid=$TARGET_PID | ${RATE} kills/s"
            fi
        else
            MISSES=$((MISSES + 1))
        fi
    else
        if [ -n "$SUP_PID" ] && ! kill -0 "$SUP_PID" 2>/dev/null; then break; fi
        NOW_MS=$(now_ms)
        if [ $(( (NOW_MS - LAST_SUCCESS_MS) / 1000 )) -gt 20 ]; then
            echo "[STALL] no killable worker for 20s"
            break
        fi
        NOT_FOUND=$((NOT_FOUND + 1))
        sleep 0.001
    fi

    sleep "$MIN_SLEEP"
done

sample_sup
RUN_END_MS=$(now_ms)
DUR_MS=$((RUN_END_MS - RUN_START_MS))
[ "$DUR_MS" -le 0 ] && DUR_MS=1

if [ -n "$SUP_PID" ]; then
    for _ in $(seq 1 600); do
        kill -0 "$SUP_PID" 2>/dev/null || break
        sleep 0.1
    done
    wait "$SUP_PID" 2>/dev/null
    SUP_EXIT=$?
fi

read_json_int() {
    [ -f "$REPORT" ] || { echo ""; return; }
    sed -n "s/.*\"$1\": *\([0-9][0-9]*\).*/\1/p" "$REPORT" 2>/dev/null | head -1
}

REAPED=$(read_json_int total_reaped)
TOKENS=$(read_json_int tokens_rescued)
CLEAN=$(read_json_int clean_settles)
FINAL_GEN=$(read_json_int final_generation)
ELAPSED_MS=$(read_json_int elapsed_ms)
DEADLOCK=$(sed -n 's/.*"deadlock": *\(true\|false\).*/\1/p' "$REPORT" 2>/dev/null | head -1)

CHECKS_FAILED=0
CHECK_LINES=""
add_check() {
    if [ "$1" = "1" ]; then
        CHECK_LINES="${CHECK_LINES}Check           : OK   $2"$'\n'
    else
        CHECK_LINES="${CHECK_LINES}Check           : FAIL $2"$'\n'
        CHECKS_FAILED=$((CHECKS_FAILED + 1))
    fi
}
if [ -n "$REAPED" ]; then
    [ "$REAPED" -eq "$EXECUTED" ] && add_check 1 "reaped == kills" || add_check 0 "reaped == kills"
    [ $((TOKENS + CLEAN)) -eq "$REAPED" ] && add_check 1 "rescue + clean == reaped" || add_check 0 "rescue + clean == reaped"
    [ "$FINAL_GEN" -eq $((EXECUTED + 1)) ] && add_check 1 "final_generation == kills + 1" || add_check 0 "final_generation == kills + 1"
    [ "$DEADLOCK" = "false" ] && add_check 1 "deadlock == false" || add_check 0 "deadlock == false"
fi
if [ -n "$SUP_PID" ]; then
    [ "$SUP_EXIT" = "0" ] && add_check 1 "supervisor exit == 0" || add_check 0 "supervisor exit == 0"
fi
if [ "$SUP_RSS_N" -gt 0 ]; then
    SUP_GROWTH_KB=$((SUP_RSS_LAST - SUP_RSS_FIRST))
    GROWTH_MB=$(awk -v k="$SUP_GROWTH_KB" 'BEGIN { printf "%+.2f", k/1024 }')
    if awk -v k="$SUP_GROWTH_KB" -v m="$MAX_GROWTH_MB" 'BEGIN { exit !(k/1024 <= m) }'; then
        add_check 1 "supervisor RSS growth $GROWTH_MB MB <= $MAX_GROWTH_MB MB"
    else
        add_check 0 "supervisor RSS growth $GROWTH_MB MB <= $MAX_GROWTH_MB MB"
    fi
fi

VERDICT="FAIL"
if [ "$EXECUTED" -eq "$KILLS" ] && [ "$CHECKS_FAILED" -eq 0 ]; then
    if [ -n "$REAPED" ]; then VERDICT="PASS"; else VERDICT="PASS (harness only, report pending)"; fi
fi

RATE=$(awk -v n="$EXECUTED" -v ms="$DUR_MS" 'BEGIN { printf "%.1f", ms > 0 ? 1000*n/ms : n }')
DUR_SEC=$(awk -v ms="$DUR_MS" 'BEGIN { printf "%.3f", ms/1000 }')

LAT_SUMMARY="n/a (nanosecond clock unavailable)"
if [ "$HAVE_NS" = 1 ] && [ -s "$LAT_FILE" ]; then
    LAT_SUMMARY=$(sort -n "$LAT_FILE" | awk '
        { a[NR]=$1; s+=$1 }
        END {
            if (NR == 0) { print "n/a"; exit }
            printf "min %.3f | p50 %.3f | p95 %.3f | p99 %.3f | p99.9 %.3f | max %.3f | avg %.3f",
                a[1], a[int((NR+1)*0.50)], a[int((NR+1)*0.95)], a[int((NR+1)*0.99)],
                a[int((NR+1)*0.999)], a[NR], s/NR
        }')
fi

SUP_RSS_AVG_KB=0
[ "$SUP_RSS_N" -gt 0 ] && SUP_RSS_AVG_KB=$((SUP_RSS_SUM / SUP_RSS_N))
W_RSS_AVG_KB=0
[ "$W_RSS_N" -gt 0 ] && W_RSS_AVG_KB=$((W_RSS_SUM / W_RSS_N))
W_THREADS_AVG=0
[ "$W_RSS_N" -gt 0 ] && W_THREADS_AVG=$(awk -v n="$W_THREADS_SUM" -v c="$W_RSS_N" 'BEGIN { printf "%.2f", n/c }')
W_FDS_AVG=0
[ "$W_RSS_N" -gt 0 ] && W_FDS_AVG=$(awk -v n="$W_FDS_SUM" -v c="$W_RSS_N" 'BEGIN { printf "%.2f", n/c }')

SUP_CPU_SEC=$(awk -v t="$SUP_CPU_TICKS" -v c="$CLK_TCK" 'BEGIN { printf "%.2f", c > 0 ? t/c : 0 }')
SUP_CPU_PCT=$(awk -v s="$SUP_CPU_SEC" -v ms="$DUR_MS" 'BEGIN { printf "%.1f", ms > 0 ? 100*s/(ms/1000) : 0 }')

echo "=========================================================="
echo "   STRESS RESULTS"
echo "=========================================================="
echo "Kills completed : $EXECUTED / $KILLS"
echo "Misses          : $MISSES   (not-found polls: $NOT_FOUND)"
echo "Duration        : ${DUR_SEC}s"
echo "Rate            : ${RATE} kills/s avg | ${PEAK_RATE} kills/s peak (500-kill window)"
echo "Kill latency ms : $LAT_SUMMARY"
if [ -n "$REAPED" ]; then
    RESCUE_PCT=$(awk -v t="$TOKENS" -v r="$REAPED" 'BEGIN { printf "%.1f", r > 0 ? 100*t/r : 0 }')
    echo "Supervisor      : reaped=$REAPED tokens_rescued=$TOKENS clean_settles=$CLEAN rescue_rate=${RESCUE_PCT}% final_gen=$FINAL_GEN elapsed_ms=$ELAPSED_MS exit=${SUP_EXIT:-n/a}"
fi
if [ "$SUP_RSS_N" -gt 0 ]; then
    SUP_GROWTH_KB=$((SUP_RSS_LAST - SUP_RSS_FIRST))
    echo "Supervisor res  : ws peak $(awk -v k="$SUP_RSS_PEAK" 'BEGIN{printf "%.2f", k/1024}') MB avg $(awk -v k="$SUP_RSS_AVG_KB" 'BEGIN{printf "%.2f", k/1024}') MB growth $(awk -v k="$SUP_GROWTH_KB" 'BEGIN{printf "%+.2f", k/1024}') MB | threads peak $SUP_THREADS_PEAK | fds peak $SUP_FDS_PEAK"
    echo "CPU             : supervisor avg ${SUP_CPU_PCT}% (${SUP_CPU_SEC}s cpu)"
fi
if [ "$W_RSS_N" -gt 0 ]; then
    echo "Worker res      : ws peak $(awk -v k="$W_RSS_PEAK" 'BEGIN{printf "%.2f", k/1024}') MB avg $(awk -v k="$W_RSS_AVG_KB" 'BEGIN{printf "%.2f", k/1024}') MB | threads peak $W_THREADS_PEAK avg $W_THREADS_AVG | fds peak $W_FDS_PEAK avg $W_FDS_AVG (samples $W_RSS_N)"
fi
printf "%s" "$CHECK_LINES"
echo "Verdict         : $VERDICT"
echo "Stats JSON      : $STATS"
echo "Report JSON     : $REPORT"
echo "=========================================================="

sup_json="null"
if [ "$SUP_RSS_N" -gt 0 ]; then
    sup_json=$(cat <<EOF2
{
    "samples": $SUP_RSS_N,
    "working_set_mb": {
      "peak": $(awk -v k="$SUP_RSS_PEAK" 'BEGIN{printf "%.3f", k/1024}'),
      "avg": $(awk -v k="$SUP_RSS_AVG_KB" 'BEGIN{printf "%.3f", k/1024}'),
      "first": $(awk -v k="$SUP_RSS_FIRST" 'BEGIN{printf "%.3f", k/1024}'),
      "last": $(awk -v k="$SUP_RSS_LAST" 'BEGIN{printf "%.3f", k/1024}'),
      "growth": $(awk -v k=$((SUP_RSS_LAST - SUP_RSS_FIRST)) 'BEGIN{printf "%.3f", k/1024}')
    },
    "threads_peak": $SUP_THREADS_PEAK,
    "fds_peak": $SUP_FDS_PEAK,
    "cpu_seconds": $SUP_CPU_SEC,
    "cpu_percent_avg": $SUP_CPU_PCT
  }
EOF2
)
fi
worker_json="null"
if [ "$W_RSS_N" -gt 0 ]; then
    worker_json=$(cat <<EOF2
{
    "samples": $W_RSS_N,
    "working_set_mb": {
      "peak": $(awk -v k="$W_RSS_PEAK" 'BEGIN{printf "%.3f", k/1024}'),
      "avg": $(awk -v k="$W_RSS_AVG_KB" 'BEGIN{printf "%.3f", k/1024}')
    },
    "threads": {
      "peak": $W_THREADS_PEAK,
      "avg": $W_THREADS_AVG
    },
    "fds": {
      "peak": $W_FDS_PEAK,
      "avg": $W_FDS_AVG
    }
  }
EOF2
)
fi

cat > "$STATS" <<EOF
{
  "timestamp_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "target_process": "$TARGET_PROCESS",
  "profile": "$PROFILE",
  "mode": "$([ "$LAUNCH" = 1 ] && echo launch || echo external)",
  "workload": "$([ "$AGGRESSIVE" = 1 ] && echo spin || echo sleep)",
  "kills_requested": $KILLS,
  "kills_completed": $EXECUTED,
  "max_growth_mb": $MAX_GROWTH_MB,
  "misses": $MISSES,
  "not_found_polls": $NOT_FOUND,
  "duration_ms": $DUR_MS,
  "rate_per_sec": $RATE,
  "peak_rate_per_sec": $PEAK_RATE,
  "supervisor": {
    "total_reaped": ${REAPED:-null},
    "tokens_rescued": ${TOKENS:-null},
    "clean_settles": ${CLEAN:-null},
    "final_generation": ${FINAL_GEN:-null},
    "elapsed_ms": ${ELAPSED_MS:-null},
    "deadlock": ${DEADLOCK:-null},
    "exit": ${SUP_EXIT:-null}
  },
  "resources": {
    "supervisor": $sup_json,
    "workers": $worker_json
  },
  "checks_failed": $CHECKS_FAILED,
  "verdict": "$VERDICT"
}
EOF

rm -f "$LAT_FILE"
case "$VERDICT" in PASS*) exit 0 ;; *) exit 1 ;; esac
