#!/bin/bash
set -e

L1_RPC_URL="${L1_RPC_URL:-http://l1-el:4545}"
OUTPUT_DIR="${OUTPUT_DIR:-/output}"
L2_CHAIN_ID="${L2_CHAIN_ID:-84538453}"
L1_CHAIN_ID="${L1_CHAIN_ID:-1337}"
L2_DATA_DIR="${L2_DATA_DIR:-/data}"
TEMPLATE_DIR="${TEMPLATE_DIR:-/templates}"
L2_BASE_AZUL_BLOCK="${L2_BASE_AZUL_BLOCK:-}"
L2_BASE_BERYL_BLOCK="${L2_BASE_BERYL_BLOCK:-}"
L2_ACTIVATION_ADMIN_ADDR="${L2_ACTIVATION_ADMIN_ADDR:-$SEQUENCER_ADDR}"
L2_EL_BOOTNODE_P2P_KEY="${L2_EL_BOOTNODE_P2P_KEY:-1111111111111111111111111111111111111111111111111111111111111111}"
L2_EL_BOOTNODE_ENODE_ID="${L2_EL_BOOTNODE_ENODE_ID:-4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1}"
L2_EL_BOOTNODE_ENODE="${L2_EL_BOOTNODE_ENODE:-enode://4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1@172.30.0.10:9303}"
L2_CL_BOOTNODE_P2P_KEY="${L2_CL_BOOTNODE_P2P_KEY:-2222222222222222222222222222222222222222222222222222222222222222}"
L2_CL_BOOTNODE_ENR_PATH="${L2_CL_BOOTNODE_ENR_PATH:-/bootnodes/cl-bootnode.enr}"

if [ -n "$L2_BASE_AZUL_BLOCK" ] && ! [[ "$L2_BASE_AZUL_BLOCK" =~ ^[0-9]+$ ]]; then
  echo "ERROR: L2_BASE_AZUL_BLOCK must be a non-negative integer when set, got: $L2_BASE_AZUL_BLOCK"
  exit 1
fi
if [ -n "$L2_BASE_BERYL_BLOCK" ] && ! [[ "$L2_BASE_BERYL_BLOCK" =~ ^[0-9]+$ ]]; then
  echo "ERROR: L2_BASE_BERYL_BLOCK must be a non-negative integer when set, got: $L2_BASE_BERYL_BLOCK"
  exit 1
fi

echo "=== L2 Genesis Generator (Live Deployment) ==="
echo "L1 RPC URL: $L1_RPC_URL"
echo "L1 Chain ID: $L1_CHAIN_ID"
echo "L2 Chain ID: $L2_CHAIN_ID"
echo "Activation admin address: $L2_ACTIVATION_ADMIN_ADDR"
if [ -n "$L2_BASE_AZUL_BLOCK" ]; then
  echo "Base Azul activation block: $L2_BASE_AZUL_BLOCK"
else
  echo "Base Azul activation block: <unset>"
fi
if [ -n "$L2_BASE_BERYL_BLOCK" ]; then
  echo "Base Beryl activation block: $L2_BASE_BERYL_BLOCK"
else
  echo "Base Beryl activation block: <unset>"
fi
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
# Deploy L1 contracts and generate L2 genesis using forge scripts
# (Uses base/contracts forge scripts instead of op-deployer)
# =============================================================================
echo ""
echo "=== Deploying via forge scripts (base/contracts) ==="

# Create working directory
WORKDIR=/contracts
echo "Working directory: $WORKDIR"

# Step 1: Generate deploy-config.json from template
echo ""
echo "--- Step 1: Generating deploy-config.json ---"
envsubst <"$TEMPLATE_DIR/deploy-config.json.template" >"$WORKDIR/deploy-config/devnet.json"
echo "Deploy config written to $WORKDIR/deploy-config/devnet.json"
cat "$WORKDIR/deploy-config/devnet.json"

# Step 2: Deploy L1 contracts via forge script
# Contracts are pre-compiled at /contracts/ (from Dockerfile.devnet).
# --slow flag sends transactions one at a time for devnet reliability.
echo ""
echo "--- Step 2: Deploying L1 contracts ---"
(
  cd /contracts
  FOUNDRY_SCRIPT_EXECUTION_PROTECTION=false \
    DEPLOY_CONFIG_PATH="$WORKDIR/deploy-config/devnet.json" \
    forge script scripts/deploy/SystemDeploy.s.sol:SystemDeploy \
    --sender "$DEPLOYER_ADDR" \
    --rpc-url "$L1_RPC_URL" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    --slow
)

# Step 3: Generate L2 genesis allocs via forge script
# Uses L2GenesisDevnet.s.sol wrapper that reads deploy-config + L1 addresses,
# constructs the L2Genesis.Input struct, runs L2Genesis, and dumps state.
echo ""
echo "--- Step 3: Generating L2 genesis allocs ---"

L2_STATE_DUMP="/contracts/state-dump-${L2_CHAIN_ID}.json"

(
  cd /contracts
  FOUNDRY_SCRIPT_EXECUTION_PROTECTION=false \
    DEPLOY_CONFIG_PATH="$WORKDIR/deploy-config/devnet.json" \
    L1_DEPLOY_ARTIFACT="$WORKDIR/deployments/${L1_CHAIN_ID}-deploy.json" \
    L2_GENESIS_STATE_DUMP="$L2_STATE_DUMP" \
    forge script scripts/L2GenesisDevnet.s.sol:L2GenesisDevnet \
    --sender "$DEPLOYER_ADDR"
)

echo "L2 genesis state dump: $L2_STATE_DUMP"

# Step 4: Extract l1-addresses.json and genesis.json
echo ""
echo "--- Step 4: Extracting artifacts ---"
L2_GENESIS_TIMESTAMP="$L1_TIMESTAMP" \
CONTRACTS_DIR=/contracts \
OUTPUT_DIR="$OUTPUT_DIR" \
L1_RPC_URL="$L1_RPC_URL" \
L1_CHAIN_ID="$L1_CHAIN_ID" \
L2_CHAIN_ID="$L2_CHAIN_ID" \
FOUNDRY_SCRIPT_EXECUTION_PROTECTION=false \
    DEPLOY_CONFIG_PATH="$WORKDIR/deploy-config/devnet.json" \
  /usr/local/bin/extract-artifacts.sh

# Step 5: Assemble rollup.json
# L2_GENESIS_HASH is not yet known (reth must init from genesis.json first).
# assemble-rollup-config.sh uses a zero placeholder; patch after reth init.
echo ""
echo "--- Step 5: Assembling rollup config ---"
OUTPUT_DIR="$OUTPUT_DIR" \
L1_RPC_URL="$L1_RPC_URL" \
L2_CHAIN_ID="$L2_CHAIN_ID" \
  /usr/local/bin/assemble-rollup-config.sh

TMP_GENESIS=$(mktemp)
jq \
  --arg activation_admin "$L2_ACTIVATION_ADMIN_ADDR" \
  '.config.activationAdminAddress = $activation_admin' \
  "$OUTPUT_DIR/genesis.json" \
  >"$TMP_GENESIS"
mv "$TMP_GENESIS" "$OUTPUT_DIR/genesis.json"
echo "Patched activation admin into genesis config"

L2_BLOCK_TIME=$(jq -re '.block_time' "$OUTPUT_DIR/rollup.json")
L2_GENESIS_TIME=$(jq -re '.genesis.l2_time' "$OUTPUT_DIR/rollup.json")
if [ -n "$L2_BASE_AZUL_BLOCK" ]; then
  L2_BASE_AZUL_TIME=$((L2_GENESIS_TIME + L2_BLOCK_TIME * L2_BASE_AZUL_BLOCK))

  echo ""
  echo "=== Configuring Base Azul Activation ==="
  echo "L2 genesis time: $L2_GENESIS_TIME"
  echo "L2 block time: $L2_BLOCK_TIME"
  echo "Base Azul activation block: $L2_BASE_AZUL_BLOCK"
  echo "Derived Base Azul activation timestamp: $L2_BASE_AZUL_TIME"

  TMP_ROLLUP=$(mktemp)
  jq \
    --argjson azul_time "$L2_BASE_AZUL_TIME" \
    '.base = ((.base // {}) + {azul: $azul_time})' \
    "$OUTPUT_DIR/rollup.json" \
    >"$TMP_ROLLUP"
  mv "$TMP_ROLLUP" "$OUTPUT_DIR/rollup.json"

  TMP_GENESIS=$(mktemp)
  jq \
    --argjson azul_time "$L2_BASE_AZUL_TIME" \
    '.config.osakaTime = $azul_time
    | .config.base = ((.config.base // {}) + {azul: $azul_time})' \
    "$OUTPUT_DIR/genesis.json" \
    >"$TMP_GENESIS"
  mv "$TMP_GENESIS" "$OUTPUT_DIR/genesis.json"

  echo "Patched Base Azul activation into rollup and genesis configs"
else
  echo ""
  echo "=== Configuring Base Azul Activation ==="
  echo "L2 genesis time: $L2_GENESIS_TIME"
  echo "L2 block time: $L2_BLOCK_TIME"
  echo "Base Azul activation block is unset; leaving base.azul and osakaTime unchanged"
fi

if [ -n "$L2_BASE_BERYL_BLOCK" ]; then
  L2_BASE_BERYL_TIME=$((L2_GENESIS_TIME + L2_BLOCK_TIME * L2_BASE_BERYL_BLOCK))

  echo ""
  echo "=== Configuring Base Beryl Activation ==="
  echo "L2 genesis time: $L2_GENESIS_TIME"
  echo "L2 block time: $L2_BLOCK_TIME"
  echo "Base Beryl activation block: $L2_BASE_BERYL_BLOCK"
  echo "Derived Base Beryl activation timestamp: $L2_BASE_BERYL_TIME"

  TMP_ROLLUP=$(mktemp)
  jq \
    --argjson beryl_time "$L2_BASE_BERYL_TIME" \
    '.base = ((.base // {}) + {beryl: $beryl_time})' \
    "$OUTPUT_DIR/rollup.json" \
    >"$TMP_ROLLUP"
  mv "$TMP_ROLLUP" "$OUTPUT_DIR/rollup.json"

  TMP_GENESIS=$(mktemp)
  jq \
    --argjson beryl_time "$L2_BASE_BERYL_TIME" \
    '.config.base = ((.config.base // {}) + {beryl: $beryl_time})' \
    "$OUTPUT_DIR/genesis.json" \
    >"$TMP_GENESIS"
  mv "$TMP_GENESIS" "$OUTPUT_DIR/genesis.json"

  echo "Patched Base Beryl activation into rollup and genesis configs"
else
  echo ""
  echo "=== Configuring Base Beryl Activation ==="
  echo "L2 genesis time: $L2_GENESIS_TIME"
  echo "L2 block time: $L2_BLOCK_TIME"
  echo "Base Beryl activation block is unset; leaving base.beryl unchanged"
fi

echo "Writing rollup-conductor.json (base fields stripped for op-conductor compatibility)..."
jq 'del(.base, .granite_channel_timeout)' "$OUTPUT_DIR/rollup.json" >"$OUTPUT_DIR/rollup-conductor.json"
echo "rollup-conductor.json written to $OUTPUT_DIR/rollup-conductor.json"

# Verify the rollup.json has the correct L1 genesis hash
ROLLUP_L1_HASH=$(jq -r '.genesis.l1.hash' "$OUTPUT_DIR/rollup.json")
echo ""
echo "=== Verifying L1 Genesis Hash ==="
echo "Actual L1 genesis hash: $L1_HASH"
echo "Rollup.json L1 hash:    $ROLLUP_L1_HASH"

if [ "$L1_HASH" != "$ROLLUP_L1_HASH" ]; then
  echo "WARNING: L1 genesis hash mismatch!"
  echo "This might cause issues with the consensus node."
else
  echo "L1 genesis hash matches!"
fi

# =============================================================================
# Generate P2P Keys for Builder
# =============================================================================
echo ""
echo "=== Generating P2P Keys ==="

echo "$BUILDER_P2P_KEY" >"$OUTPUT_DIR/builder-p2p-key.txt"
echo "$BUILDER_ENODE_ID" >"$OUTPUT_DIR/builder-enode-id.txt"
printf "%s" "$L2_EL_BOOTNODE_P2P_KEY" >"$OUTPUT_DIR/el-bootnode-p2p-key.txt"
echo "$L2_EL_BOOTNODE_ENODE_ID" >"$OUTPUT_DIR/el-bootnode-enode-id.txt"
echo "$L2_EL_BOOTNODE_ENODE" >"$OUTPUT_DIR/el-bootnode-enode.txt"
printf "%s" "$L2_CL_BOOTNODE_P2P_KEY" >"$OUTPUT_DIR/cl-bootnode-p2p-key.txt"
echo "$L2_CL_BOOTNODE_ENR_PATH" >"$OUTPUT_DIR/cl-bootnode-enr-path.txt"
echo "$SEQ1_P2P_KEY" >"$OUTPUT_DIR/sequencer-1-p2p-key.txt"
echo "$SEQ2_P2P_KEY" >"$OUTPUT_DIR/sequencer-2-p2p-key.txt"

echo "Builder P2P key written to $OUTPUT_DIR/builder-p2p-key.txt"
echo "Builder enode ID: $BUILDER_ENODE_ID"
echo "EL bootnode P2P key written to $OUTPUT_DIR/el-bootnode-p2p-key.txt"
echo "EL bootnode enode: $L2_EL_BOOTNODE_ENODE"
echo "CL bootnode P2P key written to $OUTPUT_DIR/cl-bootnode-p2p-key.txt"
echo "CL bootnode ENR path: $L2_CL_BOOTNODE_ENR_PATH"
echo "Sequencer-1 P2P key written to $OUTPUT_DIR/sequencer-1-p2p-key.txt"
echo "Sequencer-2 P2P key written to $OUTPUT_DIR/sequencer-2-p2p-key.txt"

# Cleanup
# Workdir is /contracts, no cleanup needed

echo ""
echo "=== L2 Genesis Generation Complete ==="
echo ""
echo "Files generated:"
echo "  L2 genesis: $OUTPUT_DIR/genesis.json"
echo "  Rollup config: $OUTPUT_DIR/rollup.json"
echo "  Rollup config (conductor): $OUTPUT_DIR/rollup-conductor.json"
echo "  L1 addresses: $OUTPUT_DIR/l1-addresses.json"
echo "  Builder P2P key: $OUTPUT_DIR/builder-p2p-key.txt"
echo "  EL bootnode P2P key: $OUTPUT_DIR/el-bootnode-p2p-key.txt"
echo "  CL bootnode P2P key: $OUTPUT_DIR/cl-bootnode-p2p-key.txt"
echo ""
echo "L2 Role assignments:"
echo "  Deployer:   $DEPLOYER_ADDR"
echo "  Sequencer:  $SEQUENCER_ADDR"
echo "  Batcher:    $BATCHER_ADDR"
echo "  Proposer:   $PROPOSER_ADDR"
echo "  Challenger: $CHALLENGER_ADDR"
