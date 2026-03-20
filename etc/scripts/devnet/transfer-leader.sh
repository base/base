#!/bin/bash
# Simulates a sequencer failover: stops the active sequencer on the current
# raft leader's CL node via admin_stopSequencer, then transfers raft leadership
# via op-conductor, and waits to confirm the new leader.
#
# Usage:
#   transfer-leader.sh          # transfer to any available node
#   transfer-leader.sh 0        # transfer to op-conductor-0 (base-builder-cl)
#   transfer-leader.sh 1        # transfer to op-conductor-1 (base-sequencer-1-cl)
#   transfer-leader.sh 2        # transfer to op-conductor-2 (base-sequencer-2-cl)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEVNET_ENV="$SCRIPT_DIR/../../docker/devnet-env"

if [ -f "$DEVNET_ENV" ]; then
  # shellcheck source=/dev/null
  source "$DEVNET_ENV"
fi

CONDUCTOR0_RPC_PORT="${CONDUCTOR0_RPC_PORT:-6545}"
CONDUCTOR1_RPC_PORT="${CONDUCTOR1_RPC_PORT:-6546}"
CONDUCTOR2_RPC_PORT="${CONDUCTOR2_RPC_PORT:-6547}"
CONDUCTOR0_RAFT_PORT="${CONDUCTOR0_RAFT_PORT:-5050}"
CONDUCTOR1_RAFT_PORT="${CONDUCTOR1_RAFT_PORT:-5051}"
CONDUCTOR2_RAFT_PORT="${CONDUCTOR2_RAFT_PORT:-5052}"
L2_BUILDER_CL_RPC_PORT="${L2_BUILDER_CL_RPC_PORT:-7549}"
L2_SEQ1_CL_RPC_PORT="${L2_SEQ1_CL_RPC_PORT:-10549}"
L2_SEQ2_CL_RPC_PORT="${L2_SEQ2_CL_RPC_PORT:-11549}"

CONDUCTOR_PORTS=("$CONDUCTOR0_RPC_PORT" "$CONDUCTOR1_RPC_PORT" "$CONDUCTOR2_RPC_PORT")
CONDUCTOR_SERVER_IDS=("sequencer-0" "sequencer-1" "sequencer-2")
CONDUCTOR_RAFT_ADDRS=("op-conductor-0:$CONDUCTOR0_RAFT_PORT" "op-conductor-1:$CONDUCTOR1_RAFT_PORT" "op-conductor-2:$CONDUCTOR2_RAFT_PORT")
CONDUCTOR_NAMES=("op-conductor-0" "op-conductor-1" "op-conductor-2")
CONDUCTOR_CL_NAMES=("base-builder-cl" "base-sequencer-1-cl" "base-sequencer-2-cl")
CONDUCTOR_CL_PORTS=("$L2_BUILDER_CL_RPC_PORT" "$L2_SEQ1_CL_RPC_PORT" "$L2_SEQ2_CL_RPC_PORT")

TARGET="${1:-}"

if [ -n "$TARGET" ] && ! [[ "$TARGET" =~ ^[0-2]$ ]]; then
  echo "ERROR: target must be 0, 1, or 2 (got: $TARGET)"
  echo ""
  echo "Usage: $0 [0|1|2]"
  echo "  0 → op-conductor-0 (base-builder-cl)"
  echo "  1 → op-conductor-1 (base-sequencer-1-cl)"
  echo "  2 → op-conductor-2 (base-sequencer-2-cl)"
  exit 1
fi

# ─── Find current leader ──────────────────────────────────────────────────────

LEADER_IDX=""
for i in 0 1 2; do
  result=$(curl -s --max-time 2 \
    -X POST -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"conductor_leader","params":[],"id":1}' \
    "http://localhost:${CONDUCTOR_PORTS[$i]}" 2>/dev/null \
    | jq -r '.result // empty' 2>/dev/null || true)
  if [ "$result" = "true" ]; then
    LEADER_IDX="$i"
    break
  fi
done

if [ -z "$LEADER_IDX" ]; then
  echo "ERROR: no leader found — is the devnet running?"
  exit 1
fi

LEADER_CONDUCTOR_PORT="${CONDUCTOR_PORTS[$LEADER_IDX]}"
LEADER_CL_NAME="${CONDUCTOR_CL_NAMES[$LEADER_IDX]}"
LEADER_CONDUCTOR_NAME="${CONDUCTOR_NAMES[$LEADER_IDX]}"

echo "Current leader: $LEADER_CONDUCTOR_NAME ($LEADER_CL_NAME)"

if [ -n "$TARGET" ] && [ "$TARGET" = "$LEADER_IDX" ]; then
  echo "Target is already the leader, nothing to do."
  exit 0
fi

# ─── Transfer raft leadership ─────────────────────────────────────────────────
# The conductor stops its own sequencer internally when it loses leadership,
# keeping its active state correctly tracked. Externally calling
# admin_stopSequencer before conductor_transferLeader breaks that state machine.

if [ -z "$TARGET" ]; then
  echo "Transferring raft leadership to any available node..."
  curl -s --max-time 5 \
    -X POST -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"conductor_transferLeader","params":[],"id":1}' \
    "http://localhost:$LEADER_CONDUCTOR_PORT" >/dev/null
else
  TARGET_NAME="${CONDUCTOR_NAMES[$TARGET]}"
  TARGET_CL="${CONDUCTOR_CL_NAMES[$TARGET]}"
  TARGET_SERVER_ID="${CONDUCTOR_SERVER_IDS[$TARGET]}"
  TARGET_RAFT_ADDR="${CONDUCTOR_RAFT_ADDRS[$TARGET]}"
  echo "Transferring raft leadership to $TARGET_NAME ($TARGET_CL)..."
  curl -s --max-time 5 \
    -X POST -H "Content-Type: application/json" \
    --data "{\"jsonrpc\":\"2.0\",\"method\":\"conductor_transferLeaderToServer\",\"params\":[\"$TARGET_SERVER_ID\",\"$TARGET_RAFT_ADDR\"],\"id\":1}" \
    "http://localhost:$LEADER_CONDUCTOR_PORT" >/dev/null
fi

# ─── Wait for new leader ──────────────────────────────────────────────────────

echo -n "Waiting for new leader"
for _ in $(seq 1 30); do
  sleep 0.5
  echo -n "."
  for i in 0 1 2; do
    result=$(curl -s --max-time 1 \
      -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"conductor_leader","params":[],"id":1}' \
      "http://localhost:${CONDUCTOR_PORTS[$i]}" 2>/dev/null \
      | jq -r '.result // empty' 2>/dev/null || true)
    if [ "$result" = "true" ] && [ "$i" != "$LEADER_IDX" ]; then
      echo ""
      echo "Leadership transferred: $LEADER_CONDUCTOR_NAME → ${CONDUCTOR_NAMES[$i]} (${CONDUCTOR_CL_NAMES[$i]})"
      exit 0
    fi
  done
done

echo ""
echo "WARNING: leadership did not transfer within 15s — check conductor logs"
exit 1
