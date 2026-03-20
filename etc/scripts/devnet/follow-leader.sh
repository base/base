#!/bin/bash
# Polls all op-conductor nodes to find the current raft leader, then streams
# logs from the corresponding sequencer CL container. Automatically switches
# when leadership changes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEVNET_ENV="$SCRIPT_DIR/../../docker/devnet-env"

# Load port configuration from devnet-env if available
if [ -f "$DEVNET_ENV" ]; then
  # shellcheck source=/dev/null
  source "$DEVNET_ENV"
fi

CONDUCTOR0_RPC_PORT="${CONDUCTOR0_RPC_PORT:-6545}"
CONDUCTOR1_RPC_PORT="${CONDUCTOR1_RPC_PORT:-6546}"
CONDUCTOR2_RPC_PORT="${CONDUCTOR2_RPC_PORT:-6547}"

CONDUCTOR_PORTS=("$CONDUCTOR0_RPC_PORT" "$CONDUCTOR1_RPC_PORT" "$CONDUCTOR2_RPC_PORT")
CONDUCTOR_CL_MAP=("base-builder-cl" "base-sequencer-1-cl" "base-sequencer-2-cl")
CONDUCTOR_NAMES=("op-conductor-0" "op-conductor-1" "op-conductor-2")

POLL_INTERVAL="${FOLLOW_LEADER_POLL_INTERVAL:-2}"

LOG_PID=""
CURRENT_LEADER=""

cleanup() {
  if [ -n "$LOG_PID" ] && kill -0 "$LOG_PID" 2>/dev/null; then
    kill "$LOG_PID" 2>/dev/null
  fi
  exit 0
}
trap cleanup INT TERM

find_leader() {
  for i in 0 1 2; do
    local port="${CONDUCTOR_PORTS[$i]}"
    local result
    result=$(curl -s --max-time 1 \
      -X POST \
      -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"conductor_leader","params":[],"id":1}' \
      "http://localhost:$port" 2>/dev/null \
      | jq -r '.result // empty' 2>/dev/null || true)
    if [ "$result" = "true" ]; then
      echo "$i"
      return
    fi
  done
}

echo "=== Following HA sequencer leader ==="
echo "Polling conductors every ${POLL_INTERVAL}s..."
echo "Press Ctrl+C to stop."
echo ""

while true; do
  LEADER_IDX=$(find_leader || true)

  if [ -z "$LEADER_IDX" ]; then
    if [ "$CURRENT_LEADER" != "__none__" ]; then
      echo "[$(date '+%H:%M:%S')] No leader found, waiting..."
      CURRENT_LEADER="__none__"
      if [ -n "$LOG_PID" ] && kill -0 "$LOG_PID" 2>/dev/null; then
        kill "$LOG_PID" 2>/dev/null
        LOG_PID=""
      fi
    fi
    sleep "$POLL_INTERVAL"
    continue
  fi

  LEADER_CL="${CONDUCTOR_CL_MAP[$LEADER_IDX]}"
  CONDUCTOR_NAME="${CONDUCTOR_NAMES[$LEADER_IDX]}"

  if [ "$LEADER_IDX" != "$CURRENT_LEADER" ]; then
    if [ -n "$LOG_PID" ] && kill -0 "$LOG_PID" 2>/dev/null; then
      kill "$LOG_PID" 2>/dev/null
      LOG_PID=""
    fi

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo " [$(date '+%H:%M:%S')] Leader: $CONDUCTOR_NAME → $LEADER_CL"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""

    CURRENT_LEADER="$LEADER_IDX"
    docker logs -f --tail=50 "$LEADER_CL" &
    LOG_PID=$!
  fi

  sleep "$POLL_INTERVAL"

  # Check if docker logs exited unexpectedly (container stopped, etc.)
  if [ -n "$LOG_PID" ] && ! kill -0 "$LOG_PID" 2>/dev/null; then
    LOG_PID=""
    CURRENT_LEADER=""
  fi
done
