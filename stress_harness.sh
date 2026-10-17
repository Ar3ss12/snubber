#!/usr/bin/env bash
# snubber chaos sniper for Linux / macOS
# High-frequency SIGKILL (-9) stress runner

TARGET_PROCESS="${1:-chaos_satellite}"
KILLS="${2:-10000}"
MIN_SLEEP_SEC="${3:-0.001}"

PID_FILE="target/chaos_worker.pid"
EXECUTED=0
LAST_KILLED_PID=0

echo "=========================================================="
echo "   SNUBBER CHAOS SNIPER (POSIX): HIGH-FREQUENCY SIGKILL   "
echo "=========================================================="
echo "Target: $TARGET_PROCESS ($KILLS kills)"
echo "Signal: kill -9 (SIGKILL)"
echo "----------------------------------------------------------"

while [ "$EXECUTED" -lt "$KILLS" ]; do
    TARGET_PID=0

    # 1. Fast path: check published worker PID
    if [ -f "$PID_FILE" ]; then
        PARSED=$(cat "$PID_FILE" 2>/dev/null | tr -d '[:space:]')
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

    # 3. Fire SIGKILL
    if [ "$TARGET_PID" -ne 0 ]; then
        if kill -9 "$TARGET_PID" 2>/dev/null; then
            EXECUTED=$((EXECUTED + 1))
            LAST_KILLED_PID="$TARGET_PID"
            if [ $((EXECUTED % 500)) -eq 0 ] || [ "$EXECUTED" -le 10 ] || [ "$EXECUTED" -eq "$KILLS" ]; then
                echo "[SNIPER] Executed #$EXECUTED / $KILLS | Slain PID: $TARGET_PID"
            fi
        fi
    fi

    sleep "$MIN_SLEEP_SEC"
done

echo "=========================================================="
echo ">>> [SNIPER] Completed $EXECUTED kills. Mission accomplished."
echo "=========================================================="
