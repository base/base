#!/bin/bash
set -e

L1_RPC_URL="${L1_RPC_URL:-http://l1-el:4545}"
OUTPUT_DIR="${OUTPUT_DIR:-/output}"
L2_CHAIN_ID="${L2_CHAIN_ID:-84538453}"
L1_CHAIN_ID="${L1_CHAIN_ID:-1337}"
L2_DATA_DIR="${L2_DATA_DIR:-/data}"
TEMPLATE_DIR="${TEMPLATE_DIR:-/templates}"

# Skip if L2 genesis already exists (for restarts)
if [ -f "$OUTPUT_DIR/genesis.json" ] && [ -f "$OUTPUT_DIR/rollup.json" ]; then
  echo "=== L2 Genesis already exists, skipping generation ==="
  exit 0
fi

echo "=== L2 Genesis Generator (Live Deployment) ==="
echo "L1 RPC URL: $L1_RPC_URL"
echo "L1 Chain ID: $L1_CHAIN_ID"
echo "L2 Chain ID: $L2_CHAIN_ID"
echo "Output directory: $OUTPUT_DIR"

# Wait for L1 RPC to be available
echo ""
echo "=== Waiting for L1 RPC ==="
MAX_RETRIES=100
RETRY_COUNT=0
until curl -s --max-time 2 -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  "$L1_RPC_URL" | jq -e '.result' >/dev/null 2>&1; do
  RETRY_COUNT=$((RETRY_COUNT + 1))
  if [ $RETRY_COUNT -ge $MAX_RETRIES ]; then
    echo "ERROR: L1 RPC not ready after $MAX_RETRIES retries"
    exit 1
  fi
  sleep 0.2
done
echo "L1 RPC is ready"

# Get actual L1 genesis block info
echo ""
echo "=== Getting L1 Genesis Info ==="
L1_GENESIS=$(curl -s -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["0x0", true],"id":1}' \
  "$L1_RPC_URL" | jq '.result')
L1_HASH=$(echo "$L1_GENESIS" | jq -r '.hash')
L1_TIMESTAMP=$(echo "$L1_GENESIS" | jq -r '.timestamp')
echo "L1 genesis hash: $L1_HASH"
echo "L1 genesis timestamp: $L1_TIMESTAMP"

# Create output directory
mkdir -p "$OUTPUT_DIR"

# =============================================================================
# Run op-deployer in Live Mode
# =============================================================================
echo ""
echo "=== Running op-deployer (Live Mode) ==="

# Create working directory for op-deployer
OP_DEPLOYER_WORKDIR=$(mktemp -d)
echo "op-deployer working directory: $OP_DEPLOYER_WORKDIR"

# Initialize op-deployer with custom intent type
echo "Running op-deployer init..."
op-deployer init \
  --l1-chain-id "$L1_CHAIN_ID" \
  --l2-chain-ids "$L2_CHAIN_ID" \
  --intent-type custom \
  --workdir "$OP_DEPLOYER_WORKDIR"

# Configure intent.toml for devnet using template
INTENT_FILE="$OP_DEPLOYER_WORKDIR/intent.toml"
echo "Configuring intent.toml for devnet..."

# Convert L2 chain ID to hex (0x prefixed, 32 bytes padded)
L2_CHAIN_ID_HEX=$(printf "0x%064x" $L2_CHAIN_ID)

# Export variables for envsubst
export L1_CHAIN_ID L2_CHAIN_ID_HEX DEPLOYER_ADDR SEQUENCER_ADDR BATCHER_ADDR PROPOSER_ADDR CHALLENGER_ADDR

envsubst < "$TEMPLATE_DIR/l2-intent.toml.template" > "$INTENT_FILE"

echo "Intent configured:"
cat "$INTENT_FILE"

# Run op-deployer apply with LIVE deployment target
# This deploys contracts to the running L1
echo ""
echo "Running op-deployer apply (live mode)..."
op-deployer apply \
  --workdir "$OP_DEPLOYER_WORKDIR" \
  --deployment-target live \
  --l1-rpc-url "$L1_RPC_URL" \
  --private-key "$DEPLOYER_KEY"

# Check for output files
if [ ! -f "$OP_DEPLOYER_WORKDIR/state.json" ]; then
  echo "ERROR: op-deployer did not create state.json"
  ls -la "$OP_DEPLOYER_WORKDIR"
  exit 1
fi

echo "op-deployer state.json created successfully"

# =============================================================================
# Extract L2 Genesis and Rollup Config
# =============================================================================
echo ""
echo "=== Extracting L2 Configs ==="

# Use op-deployer inspect commands to extract the data
echo "Extracting L2 genesis..."
op-deployer inspect genesis \
  --workdir "$OP_DEPLOYER_WORKDIR" \
  "$L2_CHAIN_ID" \
  > "$OUTPUT_DIR/genesis.json"
echo "L2 genesis written to $OUTPUT_DIR/genesis.json"

echo "Extracting rollup config..."
op-deployer inspect rollup \
  --workdir "$OP_DEPLOYER_WORKDIR" \
  "$L2_CHAIN_ID" \
  > "$OUTPUT_DIR/rollup.json"
echo "Rollup config written to $OUTPUT_DIR/rollup.json"

echo "Extracting L1 addresses..."
op-deployer inspect l1 \
  --workdir "$OP_DEPLOYER_WORKDIR" \
  "$L2_CHAIN_ID" \
  > "$OUTPUT_DIR/l1-addresses.json"
echo "L1 addresses written to $OUTPUT_DIR/l1-addresses.json"

# Verify the rollup.json has the correct L1 genesis hash
ROLLUP_L1_HASH=$(jq -r '.genesis.l1.hash' "$OUTPUT_DIR/rollup.json")
echo ""
echo "=== Verifying L1 Genesis Hash ==="
echo "Actual L1 genesis hash: $L1_HASH"
echo "Rollup.json L1 hash:    $ROLLUP_L1_HASH"

if [ "$L1_HASH" != "$ROLLUP_L1_HASH" ]; then
  echo "WARNING: L1 genesis hash mismatch!"
  echo "This might cause issues with the op-node."
else
  echo "L1 genesis hash matches!"
fi

# =============================================================================
# Generate P2P Keys for Builder
# =============================================================================
echo ""
echo "=== Generating P2P Keys ==="

echo "$BUILDER_P2P_KEY" > "$OUTPUT_DIR/builder-p2p-key.txt"
echo "$BUILDER_ENODE_ID" > "$OUTPUT_DIR/builder-enode-id.txt"

echo "Builder P2P key written to $OUTPUT_DIR/builder-p2p-key.txt"
echo "Builder enode ID: $BUILDER_ENODE_ID"

# Cleanup
rm -rf "$OP_DEPLOYER_WORKDIR"

echo ""
echo "=== L2 Genesis Generation Complete ==="
echo ""
echo "Files generated:"
echo "  L2 genesis: $OUTPUT_DIR/genesis.json"
echo "  Rollup config: $OUTPUT_DIR/rollup.json"
echo "  L1 addresses: $OUTPUT_DIR/l1-addresses.json"
echo "  Builder P2P key: $OUTPUT_DIR/builder-p2p-key.txt"
echo ""
echo "L2 Role assignments:"
echo "  Deployer:   $DEPLOYER_ADDR"
echo "  Sequencer:  $SEQUENCER_ADDR"
echo "  Batcher:    $BATCHER_ADDR"
echo "  Proposer:   $PROPOSER_ADDR"
echo "  Challenger: $CHALLENGER_ADDR"
