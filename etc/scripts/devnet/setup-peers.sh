#!/bin/bash
# Establishes EL (execution layer) P2P peering between the builder and sequencer
# nodes so that txpool transactions gossip across all active sequencers.
#
# Uses admin_nodeInfo to fetch each node's enode public key, resolves container
# hostnames to IPs (reth's admin_addPeer requires IPs, not hostnames), then
# calls admin_addPeer to connect every pair.
set -e

BUILDER_URL="${BUILDER_URL:-http://base-builder:7545}"
SEQ1_URL="${SEQ1_URL:-http://base-sequencer-1:10545}"
SEQ2_URL="${SEQ2_URL:-http://base-sequencer-2:11545}"

BUILDER_HOST="${BUILDER_HOST:-base-builder}"
SEQ1_HOST="${SEQ1_HOST:-base-sequencer-1}"
SEQ2_HOST="${SEQ2_HOST:-base-sequencer-2}"

BUILDER_P2P_PORT="${BUILDER_P2P_PORT:-7303}"
SEQ1_P2P_PORT="${SEQ1_P2P_PORT:-10303}"
SEQ2_P2P_PORT="${SEQ2_P2P_PORT:-11303}"

echo "=== EL P2P Peer Setup ==="

# reth's admin_addPeer rejects hostnames — resolve to IP via ping (busybox).
resolve_ip() {
  local hostname="$1"
  ping -c1 -W1 "$hostname" 2>/dev/null \
    | head -1 \
    | sed 's/.*(\([0-9.]*\)).*/\1/'
}

get_enode_pubkey() {
  local url="$1"
  local name="$2"
  local max_retries=120
  local count=0

  echo "Waiting for admin_nodeInfo from $name..." >&2
  while true; do
    result=$(curl -sf --max-time 2 -X POST "$url" \
      -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","method":"admin_nodeInfo","params":[],"id":1}' \
      2>/dev/null | jq -r '.result.enode // empty' 2>/dev/null || true)

    if [ -n "$result" ]; then
      # Extract just the pubkey: everything between enode:// and @
      echo "$result" | sed 's/enode:\/\/\([^@]*\)@.*/\1/'
      return 0
    fi

    count=$((count + 1))
    if [ "$count" -ge "$max_retries" ]; then
      echo "ERROR: could not get enode for $name after $max_retries attempts" >&2
      return 1
    fi
    sleep 0.5
  done
}

add_peer() {
  local url="$1"
  local enode="$2"
  local from_name="$3"
  local to_name="$4"

  result=$(curl -sf --max-time 2 -X POST "$url" \
    -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"admin_addPeer\",\"params\":[\"$enode\"],\"id\":1}" \
    2>/dev/null || echo '{}')

  if echo "$result" | jq -e '.result == true' >/dev/null 2>&1; then
    echo "  $from_name -> $to_name: ok"
  else
    echo "  $from_name -> $to_name: $(echo "$result" | jq -r '.error.message // .result // "unknown error"')"
  fi
}

echo "Resolving container IPs..."
BUILDER_IP=$(resolve_ip "$BUILDER_HOST")
SEQ1_IP=$(resolve_ip "$SEQ1_HOST")
SEQ2_IP=$(resolve_ip "$SEQ2_HOST")

echo "  $BUILDER_HOST -> $BUILDER_IP"
echo "  $SEQ1_HOST -> $SEQ1_IP"
echo "  $SEQ2_HOST -> $SEQ2_IP"
echo ""

BUILDER_PUBKEY=$(get_enode_pubkey "$BUILDER_URL" "base-builder")
SEQ1_PUBKEY=$(get_enode_pubkey "$SEQ1_URL" "base-sequencer-1")
SEQ2_PUBKEY=$(get_enode_pubkey "$SEQ2_URL" "base-sequencer-2")

BUILDER_ENODE="enode://${BUILDER_PUBKEY}@${BUILDER_IP}:${BUILDER_P2P_PORT}"
SEQ1_ENODE="enode://${SEQ1_PUBKEY}@${SEQ1_IP}:${SEQ1_P2P_PORT}"
SEQ2_ENODE="enode://${SEQ2_PUBKEY}@${SEQ2_IP}:${SEQ2_P2P_PORT}"

echo "Enode URLs:"
echo "  builder:     $BUILDER_ENODE"
echo "  sequencer-1: $SEQ1_ENODE"
echo "  sequencer-2: $SEQ2_ENODE"
echo ""

echo "Establishing EL P2P peers..."
add_peer "$BUILDER_URL" "$SEQ1_ENODE" "builder" "sequencer-1"
add_peer "$BUILDER_URL" "$SEQ2_ENODE" "builder" "sequencer-2"
add_peer "$SEQ1_URL"   "$BUILDER_ENODE" "sequencer-1" "builder"
add_peer "$SEQ1_URL"   "$SEQ2_ENODE"   "sequencer-1" "sequencer-2"
add_peer "$SEQ2_URL"   "$BUILDER_ENODE" "sequencer-2" "builder"
add_peer "$SEQ2_URL"   "$SEQ1_ENODE"   "sequencer-2" "sequencer-1"

echo ""
echo "=== EL P2P peer setup complete ==="
