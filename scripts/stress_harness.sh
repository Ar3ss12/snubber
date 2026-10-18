#!/usr/bin/env bash
# snubber process termination stress harness for Linux / macOS
# High-frequency SIGKILL (-9) lifecycle verification

TARGET_PROCESS="${1:-process_termination_loop}"
KILLS="${2:-10000}"
MIN_SLEEP_SEC="${3:-0.001}"

PID_FILE="target/supervised_worker.pid"
LEGACY_PID_FILE="target/chaos_worker.pid"
EXECUTED=0
LAST_KILLED_PID=0

echo "=========================================================="
echo "   SNUBBER STRESS HARNESS: FORCED PROCESS TERMINATION     "
echo "=========================================================="
echo "Target: $TARGET_PROCESS ($KILLS termination cycles)"
echo "Signal: kill -9 (SIGKILL)"
echo "----------------------------------------------------------"

while [ "$EXECUTED" -lt "$KILLS" ]; do
    TARGET_PID=0

    # 1. Fast path: check published worker PID
    CURRENT_PID_FILE=""
    if [ -f "$PID_FILE" ]; then
        CURRENT_PID_FILE="$PID_FILE"
    elif [ -f "$LEGACY_PID_FILE" ]; then
        CURRENT_PID_FILE="$LEGACY_PID_FILE"
    fi

    if [ -n "$CURRENT_PID_FILE" ]; then
        PARSED=$(cat "$CURRENT_PID_FILE" 2>/dev/null | tr -d '[:space:]')
        if [ -n "$PARSED" ] && [ "$PARSED" -ne "$LAST_KILLED_PID" ]; then
            if kill -0 "$PARSED" 2>/dev/null; then
                TARGET_PID="$PARSED"
            fi
        fi
    fi

    # 2. Fallback: pgrep newest worker
    if [ "$TARGET_PID" -eq 0 ]; then
        CANDIDATE=$(pgrep -n -f "$TARGET_PROCESS" 2>/dev/null)
        if [ -n "$CANDIDATE" ] && [ "$CANDIDATE" -ne "$LAST_KILLED_PID" ] && [ "$CANDIDATE" -ne "$$" ]; then
            TARGET_PID="$CANDIDATE"
        fi
    fi

    # 3. Deliver SIGKILL
    if [ "$TARGET_PID" -ne 0 ]; then
        if kill -9 "$TARGET_PID" 2>/dev/null; then
            EXECUTED=$((EXECUTED + 1))
            LAST_KILLED_PID="$TARGET_PID"
            if [ $((EXECUTED % 500)) -eq 0 ] || [ "$EXECUTED" -le 10 ] || [ "$EXECUTED" -eq "$KILLS" ]; then
                echo "[HARNESS] Progress: #$EXECUTED / $KILLS | Terminated PID: $TARGET_PID"
            fi
        fi
    fi

    sleep "$MIN_SLEEP_SEC"
done

echo "=========================================================="
echo "[HARNESS] Completed $EXECUTED terminations. Verification clean."
echo "=========================================================="
