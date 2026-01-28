#!/bin/bash
set -e

L1_RPC_URL="${L1_RPC_URL:-http://l1-el:4545}"
OUTPUT_DIR="${OUTPUT_DIR:-/output}"
L2_CHAIN_ID="${L2_CHAIN_ID:-84538453}"
L1_CHAIN_ID="${L1_CHAIN_ID:-1337}"
L2_DATA_DIR="${L2_DATA_DIR:-/data}"
TEMPLATE_DIR="${TEMPLATE_DIR:-/templates}"

# Skip if L2 genesis already exists (for restarts)
if [ -f "$OUTPUT_DIR/l2/genesis.json" ] && [ -f "$OUTPUT_DIR/l2/rollup.json" ]; then
  echo "=== L2 Genesis already exists, skipping generation ==="
  exit 0
fi

# Clean up any partial/stale L2 data for fresh start
echo "=== Cleaning up existing L2 data ==="
rm -rf "${OUTPUT_DIR:?}"/l2/*
rm -rf "${L2_DATA_DIR:?}"/l2-builder/*
rm -rf "${L2_DATA_DIR:?}"/l2-builder-cl/*
rm -rf "${L2_DATA_DIR:?}"/l2-client/*
rm -rf "${L2_DATA_DIR:?}"/l2-client-cl/*

# Anvil accounts
ANVIL_ACCOUNTS=(
  "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
  "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
  "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
  "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
  "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
  "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
  "0x976EA74026E726554dB657fA54763abd0C3a0aa9"
  "0x14dC79964da2C08b23698B3D3cc7Ca32193d9955"
  "0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f"
  "0xa0Ee7A142d267C1f36714E4a8F75612F20a79720"
)

# Key accounts from Anvil
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
DEPLOYER_ADDR="${ANVIL_ACCOUNTS[0]}"  # 0xf39F...2266
SEQUENCER_ADDR="${ANVIL_ACCOUNTS[5]}" # 0x9965...0A4dc (Account 5)
BATCHER_ADDR="${ANVIL_ACCOUNTS[6]}"   # 0x976E...9720 (Account 6)
PROPOSER_ADDR="${ANVIL_ACCOUNTS[7]}"  # 0x14dC...9955 (Account 7)
CHALLENGER_ADDR="${ANVIL_ACCOUNTS[8]}" # 0x2361...1E8f (Account 8)

echo "=== L2 Genesis Generator (Live Deployment) ==="
echo "L1 RPC URL: $L1_RPC_URL"
echo "L1 Chain ID: $L1_CHAIN_ID"
echo "L2 Chain ID: $L2_CHAIN_ID"
echo "Output directory: $OUTPUT_DIR"

# Wait for L1 RPC to be available
echo ""
echo "=== Waiting for L1 RPC ==="
until curl -s -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  "$L1_RPC_URL" | jq -e '.result' >/dev/null 2>&1; do
  echo "L1 RPC not ready, waiting..."
  sleep 1
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

# Create L2 output directory
L2_OUTPUT_DIR="$OUTPUT_DIR/l2"
mkdir -p "$L2_OUTPUT_DIR"

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
  > "$L2_OUTPUT_DIR/genesis.json"
echo "L2 genesis written to $L2_OUTPUT_DIR/genesis.json"

echo "Extracting rollup config..."
op-deployer inspect rollup \
  --workdir "$OP_DEPLOYER_WORKDIR" \
  "$L2_CHAIN_ID" \
  > "$L2_OUTPUT_DIR/rollup.json"
echo "Rollup config written to $L2_OUTPUT_DIR/rollup.json"

echo "Extracting L1 addresses..."
op-deployer inspect l1 \
  --workdir "$OP_DEPLOYER_WORKDIR" \
  "$L2_CHAIN_ID" \
  > "$L2_OUTPUT_DIR/l1-addresses.json"
echo "L1 addresses written to $L2_OUTPUT_DIR/l1-addresses.json"

# Verify the rollup.json has the correct L1 genesis hash
ROLLUP_L1_HASH=$(jq -r '.genesis.l1.hash' "$L2_OUTPUT_DIR/rollup.json")
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

# Using Anvil Account 9's private key for P2P (for devnet only!)
BUILDER_P2P_KEY="2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
echo "$BUILDER_P2P_KEY" > "$L2_OUTPUT_DIR/builder-p2p-key.txt"

# The enode ID is the uncompressed public key (64 bytes hex, no prefix)
# Pre-computed public key for Anvil Account 9's P2P key
BUILDER_ENODE_ID="8318535b54105d4a7aae60c08fc45f9687181b4fdfc625bd1a753fa7397fed753547f11ca8696646f2f3acb08e31016afac23e630c5d11f59f61fef57b0d2aa5"
echo "$BUILDER_ENODE_ID" > "$L2_OUTPUT_DIR/builder-enode-id.txt"

echo "Builder P2P key written to $L2_OUTPUT_DIR/builder-p2p-key.txt"
echo "Builder enode ID: $BUILDER_ENODE_ID"

# Cleanup
rm -rf "$OP_DEPLOYER_WORKDIR"

echo ""
echo "=== L2 Genesis Generation Complete ==="
echo ""
echo "Files generated:"
echo "  L2 genesis: $L2_OUTPUT_DIR/genesis.json"
echo "  Rollup config: $L2_OUTPUT_DIR/rollup.json"
echo "  L1 addresses: $L2_OUTPUT_DIR/l1-addresses.json"
echo "  Builder P2P key: $L2_OUTPUT_DIR/builder-p2p-key.txt"
echo ""
echo "L2 Role assignments:"
echo "  Deployer:   ${ANVIL_ACCOUNTS[0]}"
echo "  Sequencer:  ${ANVIL_ACCOUNTS[5]}"
echo "  Batcher:    ${ANVIL_ACCOUNTS[6]}"
echo "  Proposer:   ${ANVIL_ACCOUNTS[7]}"
echo "  Challenger: ${ANVIL_ACCOUNTS[8]}"
