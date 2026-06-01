#!/bin/bash
set -e

# patch-genesis-hash.sh — Patches rollup.json and rollup-conductor.json with the
# real L2 genesis block hash, queried from the L2 EL node after it initializes.

L2_RPC_URL="${L2_RPC_URL:-http://base-builder:7545}"
CONFIG_DIR="${CONFIG_DIR:-/configs}"

echo "=== Patching L2 Genesis Hash ==="
echo "L2 RPC URL: $L2_RPC_URL"
echo "Config dir: $CONFIG_DIR"

# Wait for L2 EL to be ready
echo "Waiting for L2 EL node..."
MAX_RETRIES=60
RETRY_COUNT=0
until curl -s --max-time 2 -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  "$L2_RPC_URL" | jq -e '.result' >/dev/null 2>&1; do
  RETRY_COUNT=$((RETRY_COUNT + 1))
  if [ $RETRY_COUNT -ge $MAX_RETRIES ]; then
    echo "ERROR: L2 EL not ready after $MAX_RETRIES retries"
    exit 1
  fi
  sleep 0.5
done
echo "L2 EL is ready"

# Get the genesis block hash
L2_GENESIS_HASH=$(curl -s -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["0x0", false],"id":1}' \
  "$L2_RPC_URL" | jq -r '.result.hash')

echo "L2 genesis hash: $L2_GENESIS_HASH"

if [ -z "$L2_GENESIS_HASH" ] || [ "$L2_GENESIS_HASH" = "null" ]; then
  echo "ERROR: Could not get L2 genesis hash"
  exit 1
fi

# Patch rollup.json
TMP=$(mktemp)
jq --arg hash "$L2_GENESIS_HASH" '.genesis.l2.hash = $hash' "$CONFIG_DIR/rollup.json" > "$TMP"
mv "$TMP" "$CONFIG_DIR/rollup.json"
echo "Patched rollup.json"

# Patch rollup-conductor.json
if [ -f "$CONFIG_DIR/rollup-conductor.json" ]; then
  TMP=$(mktemp)
  jq --arg hash "$L2_GENESIS_HASH" '.genesis.l2.hash = $hash' "$CONFIG_DIR/rollup-conductor.json" > "$TMP"
  mv "$TMP" "$CONFIG_DIR/rollup-conductor.json"
  echo "Patched rollup-conductor.json"
fi

echo "=== Genesis Hash Patch Complete ==="
