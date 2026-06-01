#!/bin/bash
set -e

# extract-artifacts.sh — Extract l1-addresses.json and assemble genesis.json
# from forge deployment output (base/contracts).
#
# Usage:
#   CONTRACTS_DIR=/contracts OUTPUT_DIR=/output L1_RPC_URL=http://l1:4545 \
#     L2_CHAIN_ID=84538453 DEPLOY_CONFIG_PATH=/contracts/deploy-config/local.json \
#     ./extract-artifacts.sh
#
# Inputs:
#   - Deployment outfile: $CONTRACTS_DIR/deployments/<l1_chainid>-deploy.json
#     Written by Artifacts.save() during `forge script SystemDeploy.s.sol --broadcast`.
#   - State dump: produced by `forge script L2Genesis.s.sol --sig runWithStateDump()`
#     at $CONTRACTS_DIR/state-dump-<l2_chainid>-<fork>.json
#   - Deploy config: $DEPLOY_CONFIG_PATH
#
# Outputs:
#   - $OUTPUT_DIR/l1-addresses.json  — PascalCase contract address map
#   - $OUTPUT_DIR/genesis.json       — Full L2 genesis (config + alloc)

CONTRACTS_DIR="${CONTRACTS_DIR:-/contracts}"
OUTPUT_DIR="${OUTPUT_DIR:-/output}"
L1_RPC_URL="${L1_RPC_URL:-http://l1-el:4545}"
L2_CHAIN_ID="${L2_CHAIN_ID:-84538453}"
DEPLOY_CONFIG_PATH="${DEPLOY_CONFIG_PATH:-${CONTRACTS_DIR}/deploy-config/local.json}"

echo "=== Extract Artifacts ==="
echo "Contracts dir:    $CONTRACTS_DIR"
echo "Output dir:       $OUTPUT_DIR"
echo "L1 RPC URL:       $L1_RPC_URL"
echo "L2 Chain ID:      $L2_CHAIN_ID"
echo "Deploy config:    $DEPLOY_CONFIG_PATH"

# ---------------------------------------------------------------------------
# Resolve L1 chain ID — needed to locate the deployment outfile
# ---------------------------------------------------------------------------
L1_CHAIN_ID="${L1_CHAIN_ID:-}"
if [ -z "$L1_CHAIN_ID" ]; then
  echo "Fetching L1 chain ID from $L1_RPC_URL ..."
  L1_CHAIN_ID=$(cast chain-id --rpc-url "$L1_RPC_URL")
fi
echo "L1 Chain ID:      $L1_CHAIN_ID"

# ---------------------------------------------------------------------------
# Validate inputs
# ---------------------------------------------------------------------------
DEPLOY_OUTFILE="${CONTRACTS_DIR}/deployments/${L1_CHAIN_ID}-deploy.json"
if [ ! -f "$DEPLOY_OUTFILE" ]; then
  echo "ERROR: Deployment outfile not found: $DEPLOY_OUTFILE"
  echo "Run 'forge script SystemDeploy.s.sol --broadcast' first."
  exit 1
fi

if [ ! -f "$DEPLOY_CONFIG_PATH" ]; then
  echo "ERROR: Deploy config not found: $DEPLOY_CONFIG_PATH"
  exit 1
fi

mkdir -p "$OUTPUT_DIR"

# ===========================================================================
# 1. Extract l1-addresses.json
# ===========================================================================
echo ""
echo "--- Extracting l1-addresses.json ---"
echo "Source: $DEPLOY_OUTFILE"

# The deployment outfile written by Artifacts.save() already uses PascalCase
# keys matching the expected format. Extract only the proxy/admin addresses
# that downstream consumers need.
jq '{
  OptimismPortalProxy:            .OptimismPortalProxy,
  SystemConfigProxy:              .SystemConfigProxy,
  L1StandardBridgeProxy:          .L1StandardBridgeProxy,
  L1CrossDomainMessengerProxy:    .L1CrossDomainMessengerProxy,
  L1ERC721BridgeProxy:            .L1ERC721BridgeProxy,
  DisputeGameFactoryProxy:        .DisputeGameFactoryProxy,
  AnchorStateRegistryProxy:       .AnchorStateRegistryProxy,
  DelayedWETHProxy:               .DelayedWETHProxy,
  AddressManager:                 .AddressManager,
  ProxyAdmin:                     .ProxyAdmin,
  OptimismMintableERC20FactoryProxy: .OptimismMintableERC20FactoryProxy,
  SuperchainConfigProxy:          .SuperchainConfigProxy,
  ETHLockboxProxy:                .ETHLockboxProxy
}' "$DEPLOY_OUTFILE" >"$OUTPUT_DIR/l1-addresses.json"

# Verify critical addresses are present (smoke.sh checks these)
for key in OptimismPortalProxy SystemConfigProxy L1StandardBridgeProxy; do
  addr=$(jq -r ".$key // empty" "$OUTPUT_DIR/l1-addresses.json")
  if [ -z "$addr" ] || [ "$addr" = "null" ]; then
    echo "ERROR: Missing required address: $key"
    exit 1
  fi
  echo "  $key = $addr"
done

echo "L1 addresses written to $OUTPUT_DIR/l1-addresses.json"

# ===========================================================================
# 2. Assemble genesis.json
# ===========================================================================
echo ""
echo "--- Assembling genesis.json ---"

# Locate the L2Genesis state dump. L2Genesis.s.sol writes to:
#   state-dump-<l2ChainId>-<fork>.json
# Try forks in reverse order (newest first).
STATE_DUMP=""
for fork in interop jovian isthmus holocene granite fjord ecotone delta; do
  candidate="${CONTRACTS_DIR}/state-dump-${L2_CHAIN_ID}-${fork}.json"
  if [ -f "$candidate" ]; then
    STATE_DUMP="$candidate"
    echo "Found state dump: $candidate"
    break
  fi
done

# Also check the generic name without fork suffix
if [ -z "$STATE_DUMP" ]; then
  candidate="${CONTRACTS_DIR}/state-dump-${L2_CHAIN_ID}.json"
  if [ -f "$candidate" ]; then
    STATE_DUMP="$candidate"
    echo "Found state dump: $candidate"
  fi
fi

if [ -z "$STATE_DUMP" ]; then
  echo "ERROR: No L2Genesis state dump found in $CONTRACTS_DIR"
  echo "Run 'forge script L2Genesis.s.sol --sig runWithStateDump()' first."
  exit 1
fi

# Read gas limit from deploy config, fall back to default
GAS_LIMIT=$(jq -r '.l2GenesisBlockGasLimit // empty' "$DEPLOY_CONFIG_PATH" 2>/dev/null || true)
if [ -z "$GAS_LIMIT" ]; then
  GAS_LIMIT="0x3938700"
  echo "Using default gas limit: $GAS_LIMIT"
else
  # Convert decimal to hex if needed
  if [[ "$GAS_LIMIT" =~ ^[0-9]+$ ]]; then
    GAS_LIMIT=$(printf "0x%x" "$GAS_LIMIT")
  fi
  echo "Gas limit from deploy config: $GAS_LIMIT"
fi

# Read base fee from deploy config, fall back to 1 gwei
BASE_FEE=$(jq -r '.l2GenesisBlockBaseFeePerGas // empty' "$DEPLOY_CONFIG_PATH" 2>/dev/null || true)
if [ -z "$BASE_FEE" ]; then
  BASE_FEE="0x3b9aca00"
else
  if [[ "$BASE_FEE" =~ ^[0-9]+$ ]]; then
    BASE_FEE=$(printf "0x%x" "$BASE_FEE")
  fi
fi

# Build the full genesis.json by wrapping the state dump allocs in a
# standard geth genesis structure. All OP Stack hardfork timestamps are
# set to 0 (active at genesis) for devnet.
jq -n \
  --arg chain_id "$L2_CHAIN_ID" \
  --arg gas_limit "$GAS_LIMIT" \
  --arg base_fee "$BASE_FEE" \
  --arg genesis_timestamp "${L2_GENESIS_TIMESTAMP:-0x0}" \
  --slurpfile allocs "$STATE_DUMP" \
'{
  config: {
    chainId:             ($chain_id | tonumber),
    homesteadBlock:      0,
    eip150Block:         0,
    eip155Block:         0,
    eip158Block:         0,
    byzantiumBlock:      0,
    constantinopleBlock: 0,
    petersburgBlock:     0,
    istanbulBlock:       0,
    muirGlacierBlock:    0,
    berlinBlock:         0,
    londonBlock:         0,
    shanghaiTime:        0,
    cancunTime:          0,
    pragueTime:          0,
    bedrockBlock:        0,
    regolithTime:        0,
    canyonTime:          0,
    deltaTime:           0,
    ecotoneTime:         0,
    fjordTime:           0,
    graniteTime:         0,
    holoceneTime:        0,
    isthmusTime:         0,
    jovianTime:          0,
    optimism: {
      eip1559Elasticity:        6,
      eip1559Denominator:       50,
      eip1559DenominatorCanyon: 250
    }
  },
  nonce:         "0x0",
  timestamp:     $genesis_timestamp,
  extraData:     "0x",
  gasLimit:      $gas_limit,
  difficulty:    "0x1",
  mixHash:       "0x0000000000000000000000000000000000000000000000000000000000000000",
  coinbase:      "0x0000000000000000000000000000000000000000",
  baseFeePerGas: $base_fee,
  alloc:         $allocs[0]
}' >"$OUTPUT_DIR/genesis.json"

echo "Genesis written to $OUTPUT_DIR/genesis.json"

# Quick validation
ALLOC_COUNT=$(jq '.alloc | length' "$OUTPUT_DIR/genesis.json")
CONFIG_CHAIN_ID=$(jq '.config.chainId' "$OUTPUT_DIR/genesis.json")
echo "  Chain ID: $CONFIG_CHAIN_ID"
echo "  Alloc entries: $ALLOC_COUNT"

echo ""
echo "=== Artifact extraction complete ==="
