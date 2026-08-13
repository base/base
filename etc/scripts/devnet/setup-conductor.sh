#!/bin/bash
set -e

CONDUCTOR0_URL="${CONDUCTOR0_URL:-http://op-conductor-0:6545}"
CONDUCTOR1_URL="${CONDUCTOR1_URL:-http://op-conductor-1:6546}"
CONDUCTOR2_URL="${CONDUCTOR2_URL:-http://op-conductor-2:6547}"
CONDUCTOR1_RAFT_ADDR="${CONDUCTOR1_RAFT_ADDR:-op-conductor-1:5051}"
CONDUCTOR2_RAFT_ADDR="${CONDUCTOR2_RAFT_ADDR:-op-conductor-2:5052}"
BUILDER_EL_URL="${BUILDER_EL_URL:-http://base-builder:7545}"
BUILDER_CL_URL="${BUILDER_CL_URL:-http://base-builder:7549}"

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
echo "=== Starting initial raft leader sequencer ==="
for attempt in $(seq 1 120); do
  if ! unsafe_head=$(curl -fsS -X POST "$BUILDER_EL_URL" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["latest",false],"id":1}' \
    | jq -er '.result.hash'); then
    if [ "$attempt" -eq 120 ]; then
      echo "ERROR: builder execution RPC not ready after $attempt attempts" >&2
      exit 1
    fi
    sleep 0.5
    continue
  fi
  if ! response=$(curl -fsS -X POST "$BUILDER_CL_URL" \
    -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"admin_startSequencer\",\"params\":[\"$unsafe_head\"],\"id\":1}"); then
    if [ "$attempt" -eq 120 ]; then
      echo "ERROR: builder consensus RPC not ready after $attempt attempts" >&2
      exit 1
    fi
    sleep 0.5
    continue
  fi
  if echo "$response" | jq -e '.error == null' >/dev/null; then
    break
  fi
  if [ "$attempt" -eq 120 ]; then
    echo "$response" | jq -r '.error.message // "unknown admin_startSequencer error"' >&2
    exit 1
  fi
  sleep 0.5
done

echo ""
echo "=== Resuming initial raft leader conductor ==="
response=$(curl -fsS -X POST "$CONDUCTOR0_URL" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"conductor_resume","params":[],"id":1}')
if ! echo "$response" | jq -e '.error == null' >/dev/null; then
  echo "$response" | jq -r '.error.message // "unknown conductor_resume error"' >&2
  exit 1
fi

echo ""
echo "=== Conductor cluster setup complete ==="
