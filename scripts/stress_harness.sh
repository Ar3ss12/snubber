#!/usr/bin/env bash
# snubber process termination stress harness (Linux / macOS).
# Positional args (backward compatible): [target] [kills] [min_sleep_sec]
# Flags: --launch (start supervisor too), --build, --debug, --aggressive,
#        --external-supervisor (the caller launched it; required in CI)
set -u

TARGET_PROCESS="process_termination_loop"
KILLS=10000
MIN_SLEEP=0
LAUNCH=0
EXTERNAL_SUPERVISOR=0
BUILD=0
AGGRESSIVE=0
PROFILE="release"
REPORT="target/termination_report.json"
STATS="target/stress_stats.json"
SAMPLE_EVERY=5
MAX_GROWTH_MB=2.0
MAX_FD_GROWTH=16
POS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --launch) LAUNCH=1 ;;
        --external-supervisor) EXTERNAL_SUPERVISOR=1 ;;
        --build) BUILD=1 ;;
        --debug) PROFILE="debug" ;;
        --aggressive) AGGRESSIVE=1 ;;
        --max-growth-mb)
            shift
            case "${1:-}" in ''|*[!0-9.]*) echo "Error: --max-growth-mb requires a numeric value" >&2; exit 1 ;; esac
            MAX_GROWTH_MB="$1"
            ;;
        --max-fd-growth)
            shift
            case "${1:-}" in ''|*[!0-9]*) echo "Error: --max-fd-growth requires an integer" >&2; exit 1 ;; esac
            MAX_FD_GROWTH="$1"
            ;;
        *) POS+=("$1") ;;
    esac
    shift
done
TARGET_PROCESS="${POS[0]:-$TARGET_PROCESS}"
KILLS="${POS[1]:-$KILLS}"
MIN_SLEEP="${POS[2]:-$MIN_SLEEP}"

# CI mode: GitHub Actions sets CI=true (or 1). In CI a SKIP is a hole and
# "kill only" mode means the harness never controlled the supervisor it
# gates -- both must fail the run instead of reporting a soft verdict.
in_ci() {
    case "$(printf '%s' "${CI:-}" | tr '[:upper:]' '[:lower:]')" in
        true | 1) return 0 ;;
        *) return 1 ;;
    esac
}
if in_ci && [ "$LAUNCH" != 1 ] && [ "$EXTERNAL_SUPERVISOR" != 1 ]; then
    echo "[HARNESS] FAIL: CI requires --launch (or --external-supervisor when a test runner owns the supervisor); kill-only mode gates nothing" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

# Sampling backends: /proc on Linux; ps/lsof on macOS and BSD. Probe once so
# a platform without either fails the garbage-collection gates explicitly
# instead of sampling nothing and reporting PASS.
IS_LINUX=0
case "$(uname -s 2>/dev/null)" in Linux*) IS_LINUX=1 ;; esac
LSOF=""
if [ "$IS_LINUX" = 0 ]; then
    if command -v lsof > /dev/null 2>&1; then
        LSOF=lsof
    elif [ -x /usr/sbin/lsof ]; then
        LSOF=/usr/sbin/lsof
    fi
fi
FD_SAMPLING=0
[ "$IS_LINUX" = 1 ] && FD_SAMPLING=1
[ -n "$LSOF" ] && FD_SAMPLING=1

PID_FILES=("target/supervised_worker.pid" "target/chaos_worker.pid")
EXE="target/$PROFILE/examples/$TARGET_PROCESS"
# mktemp: two concurrent harness runs must not share a latency temp file.
LAT_FILE="$(mktemp "${TMPDIR:-/tmp}/snubber_stress_lat.XXXXXX")" || exit 1
: > "$LAT_FILE"
# INT/TERM/EXIT: never leave a launched supervisor or a temp file behind.
trap '[ -n "$SUP_PID" ] && kill -9 "$SUP_PID" 2>/dev/null; rm -f "$LAT_FILE"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

HAVE_NS=0
if [ -n "$(date +%s%N 2>/dev/null | tr -dc '0-9')" ] && [ "$(date +%s%N 2>/dev/null | tr -dc '0-9' | wc -c)" -ge 18 ]; then
    HAVE_NS=1
fi

now_ms() {
    if [ "$HAVE_NS" = 1 ]; then echo $(( $(date +%s%N) / 1000000 )); else echo $(( SECONDS * 1000 )); fi
}

rss_kb() {
    if [ -r "/proc/$1/status" ]; then
        awk '/^VmRSS:/ {print $2; exit}' "/proc/$1/status" 2>/dev/null
        return
    fi
    # macOS/BSD: ps rss is in KiB, same unit as VmRSS.
    ps -o rss= -p "$1" 2>/dev/null | awk 'NF {print $1; exit}'
}
threads_n() {
    if [ -r "/proc/$1/status" ]; then
        awk '/^Threads:/ {print $2; exit}' "/proc/$1/status" 2>/dev/null
        return
    fi
    ps -o thcount= -p "$1" 2>/dev/null | awk 'NF {print int($1); exit}'
}
fds_n() {
    if [ -d "/proc/$1/fd" ]; then
        ls "/proc/$1/fd" 2>/dev/null | wc -l | tr -d ' '
        return
    fi
    # lsof -F f prints one "f<fd>" line per descriptor; -nP skips lookups.
    [ -n "$LSOF" ] || return 0
    "$LSOF" -nP -p "$1" -F f 2>/dev/null | awk '/^f/ {n++} END {print n + 0}'
}

# Full /proc starttime ticks (field 22 of stat, monotonic since boot). The
# old code divided by CLK_TCK and compared SECONDS: two workers forked in
# the same second were indistinguishable, so a recycled pid could inherit
# the previous victim's identity. Ticks never wrap within a run.
LAST_KILLED_START_TICKS=0
TARGET_START_TICKS=0
# Identity gate before SIGKILL (parity with the .ps1 image+creation-time
# and .py /proc-starttime killers): the victim must BE the worker image,
# not an unrelated process whose argv merely CONTAINS the target path
# (a `tail -f` on the supervisor log, an editor, an ancestor shell).
# Linux: /proc/<pid>/exe image name + full starttime ticks vs the last
# kill (a stale recycled pid or a not-yet-rewritten pid file is skipped).
# macOS/BSD: no /proc -- image name plus lstart, whose resolution is one
# SECOND, so two workers started within the same second are refused
# rather than guessed (documented platform boundary).
target_is_worker() {
    TARGET_START_TICKS=0
    pid="$1"
    exe=$(basename "$(readlink "/proc/$pid/exe" 2>/dev/null)" 2>/dev/null)
    if [ -n "$exe" ]; then
        [ "$exe" = "$TARGET_PROCESS" ] || return 1
        start_ticks=$(sed 's/.*) //' "/proc/$pid/stat" 2>/dev/null | awk '{print $20}')
        case "$start_ticks" in ''|*[!0-9]*) return 1 ;; esac
        TARGET_START_TICKS=$start_ticks
        [ "$TARGET_START_TICKS" -gt "$LAST_KILLED_START_TICKS" ] || return 1
        return 0
    fi
    # No /proc (macOS/BSD): image name plus process creation time. Without
    # the creation-time gate a recycled pid with the same image name would
    # be SIGKILLed; if the time cannot be resolved the kill is refused.
    comm_line=$(ps -ww -o comm= -p "$pid" 2>/dev/null | tail -n 1)
    [ "$(basename "$comm_line")" = "$TARGET_PROCESS" ] || return 1
    lstart=$(ps -o lstart= -p "$pid" 2>/dev/null | sed 's/^ *//;s/ *$//')
    [ -n "$lstart" ] || return 1
    if [ "$IS_LINUX" = 1 ]; then
        start_s=$(date -d "$lstart" +%s 2>/dev/null)
    else
        start_s=$(date -j -f "%a %b %d %T %Y" "$lstart" +%s 2>/dev/null)
    fi
    case "$start_s" in ''|*[!0-9]*) return 1 ;; esac
    # One-second resolution: strictly-greater still rejects a same-second
    # respawn, which is the safe side of the ambiguity.
    TARGET_START_TICKS=$start_s
    [ "$TARGET_START_TICKS" -gt "$LAST_KILLED_START_TICKS" ] || return 1
}
cpu_ticks() {
    if [ -r "/proc/$1/stat" ]; then
        sed 's/^[^)]*) //' "/proc/$1/stat" 2>/dev/null | awk '{print $12+$13}'
        return
    fi
    # macOS/BSD: parse `ps -o time=` ([dd-]hh:mm:ss[.cc]) into ticks.
    ps -o time= -p "$1" 2>/dev/null | awk -v c="$CLK_TCK" '
        NF {
            t = $1; d = 0
            if (index(t, "-") > 0) { split(t, p, "-"); d = p[1]; t = p[2] }
            n = split(t, a, ":")
            if (n == 3) sec = a[1] * 3600 + a[2] * 60 + a[3]
            else if (n == 2) sec = a[1] * 60 + a[2]
            else sec = a[1]
            printf "%d\n", int((d * 86400 + sec) * c)
            exit
        }'
}

SUP_PID=""
SUP_EXIT=""
SUP_RSS_PEAK=0
SUP_RSS_SUM=0
SUP_RSS_N=0
SUP_RSS_FIRST=0
SUP_RSS_LAST=0
SUP_THREADS_PEAK=0
SUP_FDS_PEAK=0
SUP_FDS_N=0
SUP_FDS_FIRST=0
SUP_FDS_LAST=0
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
    if [ -n "${fds:-}" ]; then
        [ "$SUP_FDS_N" -eq 0 ] && SUP_FDS_FIRST=$fds
        SUP_FDS_LAST=$fds
        SUP_FDS_N=$((SUP_FDS_N + 1))
    fi
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
            cargo build --locked --release --example "$TARGET_PROCESS" || { echo "[FAIL] cargo build"; exit 1; }
        else
            cargo build --locked --example "$TARGET_PROCESS" || { echo "[FAIL] cargo build"; exit 1; }
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
        # Anchor on the target/<profile>/examples/ path: a bare -f match on
        # the process name also hits ancestor shells whose command line
        # contains the target string (e.g. sh -c './scripts/stress_harness.sh
        # process_termination_loop ...').
        CANDIDATE=$(pgrep -n -f "target/[^/ ]*/examples/$TARGET_PROCESS" 2>/dev/null)
        if [ -n "$CANDIDATE" ] && [ "$CANDIDATE" -ne "$LAST_KILLED_PID" ] \
            && [ "$CANDIDATE" -ne "$$" ] && [ "$CANDIDATE" -ne "$PPID" ] \
            && { [ -z "$SUP_PID" ] || [ "$CANDIDATE" -ne "$SUP_PID" ]; }; then
            TARGET_PID="$CANDIDATE"
        fi
    fi

    # Identity gate: never SIGKILL a process that is not the worker image.
    # A foreign candidate (or a stale recycled start) is skipped; if no
    # genuine worker ever appears, the STALL watchdog breaks the loop.
    if [ "$TARGET_PID" -ne 0 ] && ! target_is_worker "$TARGET_PID"; then
        TARGET_PID=0
    fi

    if [ "$TARGET_PID" -ne 0 ]; then
        START_NS=""
        [ "$HAVE_NS" = 1 ] && START_NS=$(date +%s%N)
        sample_worker "$TARGET_PID"
        if kill -9 "$TARGET_PID" 2>/dev/null; then
            LAST_KILLED_START_TICKS=$TARGET_START_TICKS
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
    # A wedged supervisor (the very failure this harness gates on) must
    # fail the run, not hang the harness forever.
    if kill -0 "$SUP_PID" 2>/dev/null; then
        echo "[HARNESS] supervisor did not exit within the grace window; killing"
        kill -9 "$SUP_PID" 2>/dev/null
    fi
    wait "$SUP_PID" 2>/dev/null
    SUP_EXIT=$?
fi

# Real JSON parser (python3), not a regex: a nested or reformatted report
# must parse or fail. An empty result is the documented fallback -- the
# four conservation checks below then fail instead of "passing harness-only".
read_json_int() {
    [ -f "$REPORT" ] || { echo ""; return; }
    command -v python3 > /dev/null 2>&1 || { echo ""; return; }
    python3 - "$REPORT" "$1" 2>/dev/null <<'PYEOF_INNER'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as handle:
        value = json.load(handle).get(sys.argv[2])
except (OSError, ValueError, AttributeError):
    sys.exit(0)
if isinstance(value, bool) or not isinstance(value, int):
    sys.exit(0)
print(value)
PYEOF_INNER
}

REAPED=$(read_json_int total_reaped)
TOKENS=$(read_json_int tokens_rescued)
CLEAN=$(read_json_int clean_settles)
FINAL_GEN=$(read_json_int final_generation)
ELAPSED_MS=$(read_json_int elapsed_ms)
AMBUSH=$(read_json_int ambush_kills)
ESCAPE=$(read_json_int escape_kills)
STILLBORN=$(read_json_int stillborn_kills)

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
# An unavailable platform probe is reported explicitly; it must not read as
# either PASS or FAIL (mirrors stress_multi_lineage.py's SKIP accounting).
add_skip() {
    CHECK_LINES="${CHECK_LINES}Check           : SKIP $1"$'\n'
}
if [ -n "$REAPED" ]; then
    [ "$REAPED" -eq "$EXECUTED" ] && add_check 1 "reaped == kills" || add_check 0 "reaped == kills"
    [ $((TOKENS + CLEAN)) -eq "$REAPED" ] && add_check 1 "rescue + clean == reaped" || add_check 0 "rescue + clean == reaped"
    [ $((AMBUSH + ESCAPE + STILLBORN)) -eq "$REAPED" ] && add_check 1 "class buckets sum to reaped" || add_check 0 "class buckets sum to reaped"
    [ "$FINAL_GEN" -eq $((EXECUTED + 1)) ] && add_check 1 "final_generation == kills + 1" || add_check 0 "final_generation == kills + 1"
else
    # A missing or unparsable report proves nothing: the four checks
    # above never ran, so the run must fail instead of "passing harness-only".
    add_check 0 "report present and parsable ($REPORT)"
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
elif [ "$LAUNCH" = "1" ]; then
    # Zero sampling turns the leak gate into a silent no-op (e.g. the
    # sampler could not read /proc): a run with no samples proves
    # nothing and must fail, not pass.
    add_check 0 "sampler collected supervisor samples (got 0)"
fi
if [ "$FD_SAMPLING" = "1" ]; then
    if [ "$SUP_FDS_N" -gt 0 ]; then
        FD_GROWTH=$((SUP_FDS_LAST - SUP_FDS_FIRST))
        [ "$FD_GROWTH" -le "$MAX_FD_GROWTH" ] && add_check 1 "supervisor fd growth $FD_GROWTH <= $MAX_FD_GROWTH" \
            || add_check 0 "supervisor fd growth $FD_GROWTH <= $MAX_FD_GROWTH"
    elif [ "$LAUNCH" = "1" ]; then
        # Descriptor sampling was available but produced nothing: the
        # leak gate must not pass on no data.
        add_check 0 "sampler collected supervisor fd samples (got 0)"
    fi
else
    if in_ci; then
        # CI provisions Linux /proc; a missing fd backend means the leak
        # gate silently stopped existing.
        add_check 0 "supervisor fd sampling available (CI requires /proc or lsof)"
    else
        add_skip "supervisor fd growth (no /proc and no lsof)"
    fi
fi

VERDICT="FAIL"
if [ "$EXECUTED" -eq "$KILLS" ] && [ "$CHECKS_FAILED" -eq 0 ] && [ -n "$REAPED" ]; then
    VERDICT="PASS"
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
    SUP_FD_GROWTH=$((SUP_FDS_LAST - SUP_FDS_FIRST))
    echo "Supervisor res  : ws peak $(awk -v k="$SUP_RSS_PEAK" 'BEGIN{printf "%.2f", k/1024}') MB avg $(awk -v k="$SUP_RSS_AVG_KB" 'BEGIN{printf "%.2f", k/1024}') MB growth $(awk -v k="$SUP_GROWTH_KB" 'BEGIN{printf "%+.2f", k/1024}') MB | threads peak $SUP_THREADS_PEAK | fds first ${SUP_FDS_FIRST:-0} peak $SUP_FDS_PEAK last ${SUP_FDS_LAST:-0} growth ${SUP_FD_GROWTH:+$SUP_FD_GROWTH}"
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
    "fds": {
      "samples": $SUP_FDS_N,
      "first": $SUP_FDS_FIRST,
      "peak": $SUP_FDS_PEAK,
      "last": $SUP_FDS_LAST,
      "growth": $((SUP_FDS_LAST - SUP_FDS_FIRST))
    },
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
  "max_fd_growth": $MAX_FD_GROWTH,
  "fd_sampling": $FD_SAMPLING,
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

# The report is the evidence for a PASS: a failed or truncated write
# (disk full, read-only target) must not fall through to exit 0.
if [ ! -s "$STATS" ]; then
    echo "[FAIL] stats report $STATS is missing or empty" >&2
    exit 1
fi

rm -f "$LAT_FILE"
case "$VERDICT" in PASS*) exit 0 ;; *) exit 1 ;; esac
