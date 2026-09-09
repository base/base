#!/bin/bash
set -euo pipefail

CONDUCTOR0_URL="${CONDUCTOR0_URL:-http://op-conductor-0:6545}"
CONDUCTOR1_URL="${CONDUCTOR1_URL:-http://op-conductor-1:6546}"
CONDUCTOR2_URL="${CONDUCTOR2_URL:-http://op-conductor-2:6547}"
CONDUCTOR1_RAFT_ADDR="${CONDUCTOR1_RAFT_ADDR:-op-conductor-1:5051}"
CONDUCTOR2_RAFT_ADDR="${CONDUCTOR2_RAFT_ADDR:-op-conductor-2:5052}"

echo "=== Conductor Cluster Setup ==="

rpc() {
  local url="$1"
  local method="$2"
  local params="$3"
  curl --fail-with-body -sS --max-time 5 -X POST "$url" \
    -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}" |
    jq -c 'if .error != null then error(.error.message)
           elif has("result") then .result else error("missing JSON-RPC result") end'
}

retry() {
  local attempt
  for ((attempt = 0; attempt < 120; attempt++)); do
    if "$@"; then return 0; fi
    sleep 0.5
  done
  echo "ERROR: conductor setup timed out: $*" >&2
  return 1
}

leader_rpc() {
  local url leader
  for url in "$CONDUCTOR0_URL" "$CONDUCTOR1_URL" "$CONDUCTOR2_URL"; do
    leader="$(rpc "$url" conductor_leader '[]')" || continue
    if [[ "$leader" == true ]]; then
      rpc "$url" "$1" "$2"
      return $?
    fi
  done
  return 1
}

verify_membership() {
  local membership
  membership="$(leader_rpc conductor_clusterMembership '[]')" || return 1
  jq -e '.servers | map(select(.suffrage == 0) | .id) | sort ==
    ["sequencer-0", "sequencer-1", "sequencer-2"]' <<<"$membership" >/dev/null || return 1
  echo "$membership"
}

for url in "$CONDUCTOR0_URL" "$CONDUCTOR1_URL" "$CONDUCTOR2_URL"; do
  retry rpc "$url" conductor_leader '[]' >/dev/null
done

echo ""
echo "=== Adding sequencer-1 as Raft voter ==="
retry leader_rpc conductor_addServerAsVoter "[\"sequencer-1\",\"$CONDUCTOR1_RAFT_ADDR\",0]"

echo ""
echo "=== Adding sequencer-2 as Raft voter ==="
retry leader_rpc conductor_addServerAsVoter "[\"sequencer-2\",\"$CONDUCTOR2_RAFT_ADDR\",0]"

echo ""
echo "=== Verifying cluster membership ==="
retry verify_membership

echo ""
echo "=== Conductor cluster setup complete ==="
