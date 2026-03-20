#!/bin/bash
set -e

CONDUCTOR0_URL="${CONDUCTOR0_URL:-http://op-conductor-0:6545}"
CONDUCTOR1_URL="${CONDUCTOR1_URL:-http://op-conductor-1:6546}"
CONDUCTOR2_URL="${CONDUCTOR2_URL:-http://op-conductor-2:6547}"
CONDUCTOR1_RAFT_ADDR="${CONDUCTOR1_RAFT_ADDR:-op-conductor-1:5051}"
CONDUCTOR2_RAFT_ADDR="${CONDUCTOR2_RAFT_ADDR:-op-conductor-2:5052}"

echo "=== Conductor Cluster Setup ==="

wait_for_rpc() {
  local url="$1"
  local name="$2"
  local max_retries=120
  local count=0
  echo "Waiting for $name at $url..."
  until curl -s --max-time 2 -X POST "$url" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"conductor_leader","params":[],"id":1}' \
    >/dev/null 2>&1; do
    count=$((count + 1))
    if [ $count -ge $max_retries ]; then
      echo "ERROR: $name not ready after $max_retries retries"
      exit 1
    fi
    sleep 0.5
  done
  echo "$name is ready"
}

wait_for_rpc "$CONDUCTOR0_URL" "op-conductor-0"
wait_for_rpc "$CONDUCTOR1_URL" "op-conductor-1"
wait_for_rpc "$CONDUCTOR2_URL" "op-conductor-2"

echo ""
echo "=== Adding sequencer-1 as Raft voter ==="
curl -s -X POST "$CONDUCTOR0_URL" \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"conductor_addServerAsVoter\",\"params\":[\"sequencer-1\",\"$CONDUCTOR1_RAFT_ADDR\",0],\"id\":1}" | jq .

echo ""
echo "=== Adding sequencer-2 as Raft voter ==="
curl -s -X POST "$CONDUCTOR0_URL" \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"conductor_addServerAsVoter\",\"params\":[\"sequencer-2\",\"$CONDUCTOR2_RAFT_ADDR\",0],\"id\":1}" | jq .

echo ""
echo "=== Verifying cluster membership ==="
curl -s -X POST "$CONDUCTOR0_URL" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"conductor_clusterMembership","params":[],"id":1}' | jq .

echo ""
echo "=== Conductor cluster setup complete ==="
