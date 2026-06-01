#!/bin/bash
set -e

# assemble-rollup-config.sh — Assembles rollup.json from multiple data sources.
#
# base/contracts does NOT produce rollup.json. This script constructs it from:
#   1. L1 RPC — genesis block hash/number
#   2. genesis.json — L2 genesis hash and timestamp
#   3. l1-addresses.json — contract addresses (OptimismPortalProxy, SystemConfigProxy)
#   4. SystemConfig on-chain state — batcher, overhead, scalar, gasLimit
#   5. Derived — batch_inbox_address from l2ChainId
#
# CRITICAL: RollupConfig uses #[serde(deny_unknown_fields)].
# ANY extra field in rollup.json will cause a hard crash at deserialization.
#
# Usage:
#   L1_RPC_URL=http://l1:4545 OUTPUT_DIR=/output L2_CHAIN_ID=84538453 \
#     ./assemble-rollup-config.sh
#
# Optional env vars for overrides:
#   L2_GENESIS_HASH   — Pre-computed L2 genesis block hash (otherwise placeholder)
#   BLOCK_TIME        — L2 block time in seconds (default: 2)
#   MAX_SEQUENCER_DRIFT — Max sequencer drift in seconds (default: 600)
#   SEQ_WINDOW_SIZE   — Sequencer window size in blocks (default: 3600)
#   CHANNEL_TIMEOUT   — Channel timeout in blocks (default: 300)
#   GRANITE_CHANNEL_TIMEOUT — Granite channel timeout (default: 50)

# ---------------------------------------------------------------------------
# Environment variables with defaults
# ---------------------------------------------------------------------------
OUTPUT_DIR="${OUTPUT_DIR:-/output}"
L1_RPC_URL="${L1_RPC_URL:-http://l1-el:4545}"
L2_CHAIN_ID="${L2_CHAIN_ID:-84538453}"

# Protocol parameters (devnet defaults)
BLOCK_TIME="${BLOCK_TIME:-2}"
MAX_SEQUENCER_DRIFT="${MAX_SEQUENCER_DRIFT:-600}"
SEQ_WINDOW_SIZE="${SEQ_WINDOW_SIZE:-3600}"
CHANNEL_TIMEOUT="${CHANNEL_TIMEOUT:-300}"
GRANITE_CHANNEL_TIMEOUT="${GRANITE_CHANNEL_TIMEOUT:-50}"

# Input files (produced by extract-artifacts.sh)
L1_ADDRESSES="${OUTPUT_DIR}/l1-addresses.json"
GENESIS_JSON="${OUTPUT_DIR}/genesis.json"

echo "=== Assemble Rollup Config ==="
echo "Output dir:       $OUTPUT_DIR"
echo "L1 RPC URL:       $L1_RPC_URL"
echo "L2 Chain ID:      $L2_CHAIN_ID"
echo "Block time:       $BLOCK_TIME"

# ---------------------------------------------------------------------------
# Validate inputs
# ---------------------------------------------------------------------------
if [ ! -f "$L1_ADDRESSES" ]; then
  echo "ERROR: l1-addresses.json not found at $L1_ADDRESSES"
  echo "Run extract-artifacts.sh first."
  exit 1
fi
if [ ! -f "$GENESIS_JSON" ]; then
  echo "ERROR: genesis.json not found at $GENESIS_JSON"
  echo "Run extract-artifacts.sh first."
  exit 1
fi

# ===========================================================================
# 1. Fetch L1 genesis info
# ===========================================================================
echo ""
echo "--- Fetching L1 genesis info ---"
L1_CHAIN_ID=$(cast chain-id --rpc-url "$L1_RPC_URL")
L1_GENESIS_HASH=$(cast block 0 --rpc-url "$L1_RPC_URL" -f hash)

echo "L1 chain ID:      $L1_CHAIN_ID"
echo "L1 genesis hash:  $L1_GENESIS_HASH"

# ===========================================================================
# 2. Extract L2 genesis info from genesis.json
# ===========================================================================
echo ""
echo "--- Extracting L2 genesis info ---"

# genesis.json stores timestamp as a hex string (e.g. "0x665ba0fc")
L2_GENESIS_TIME_HEX=$(jq -re '.timestamp' "$GENESIS_JSON")
L2_GENESIS_TIME=$(printf "%d" "$L2_GENESIS_TIME_HEX")

# L2 genesis hash: accept env override, otherwise use placeholder.
# The true genesis hash is only known after reth initializes the chain
# from genesis.json. Patch rollup.json with the real hash after reth init.
if [ -z "${L2_GENESIS_HASH:-}" ]; then
  echo "WARNING: L2_GENESIS_HASH not set — using zero placeholder."
  echo "         Patch rollup.json after reth init with the real genesis hash."
  L2_GENESIS_HASH="0x0000000000000000000000000000000000000000000000000000000000000000"
fi

echo "L2 genesis time:  $L2_GENESIS_TIME"
echo "L2 genesis hash:  $L2_GENESIS_HASH"

# ===========================================================================
# 3. Extract contract addresses from l1-addresses.json
# ===========================================================================
echo ""
echo "--- Extracting contract addresses ---"
DEPOSIT_CONTRACT=$(jq -re '.OptimismPortalProxy' "$L1_ADDRESSES")
SYSTEM_CONFIG_PROXY=$(jq -re '.SystemConfigProxy' "$L1_ADDRESSES")

echo "OptimismPortalProxy:  $DEPOSIT_CONTRACT"
echo "SystemConfigProxy:    $SYSTEM_CONFIG_PROXY"

# ===========================================================================
# 4. Read SystemConfig on-chain state
# ===========================================================================
echo ""
echo "--- Reading SystemConfig on-chain state ---"

# batcherHash() returns bytes32 — the batcher address occupies the last 20 bytes
BATCHER_HASH=$(cast call "$SYSTEM_CONFIG_PROXY" "batcherHash()(bytes32)" --rpc-url "$L1_RPC_URL")
BATCHER_ADDR="0x${BATCHER_HASH: -40}"

# overhead() and scalar() — fetch raw return data to get 32-byte padded hex strings.
# The Rust SystemConfig serializes these as full 32-byte hex via serialize_u256_full.
OVERHEAD_RAW=$(cast call "$SYSTEM_CONFIG_PROXY" "overhead()" --rpc-url "$L1_RPC_URL")
SCALAR_RAW=$(cast call "$SYSTEM_CONFIG_PROXY" "scalar()" --rpc-url "$L1_RPC_URL")

# gasLimit() — decimal number
GAS_LIMIT=$(cast call "$SYSTEM_CONFIG_PROXY" "gasLimit()(uint64)" --rpc-url "$L1_RPC_URL" | awk "{print \$1}")

echo "Batcher address:  $BATCHER_ADDR"
echo "Overhead:         $OVERHEAD_RAW"
echo "Scalar:           $SCALAR_RAW"
echo "Gas limit:        $GAS_LIMIT"

# ===========================================================================
# 5. Compute batch_inbox_address from l2ChainId
# ===========================================================================
echo ""
echo "--- Computing batch_inbox_address ---"

# Solidity: address(uint160(uint256(keccak256(abi.encode("]]]]", _chainId)))))
# Steps: ABI-encode string + uint256, keccak256, take last 20 bytes as address
ENCODED=$(cast abi-encode "f(string,uint256)" "]]]]" "$L2_CHAIN_ID")
INBOX_HASH=$(cast keccak "$ENCODED")
BATCH_INBOX="0x${INBOX_HASH: -40}"

echo "Batch inbox:      $BATCH_INBOX"

# ===========================================================================
# 6. Assemble rollup.json
# ===========================================================================
echo ""
echo "--- Assembling rollup.json ---"

# CRITICAL: Include ONLY fields defined in the RollupConfig struct.
# deny_unknown_fields means any extra field causes a hard crash.
#
# Fields included:
#   genesis              — ChainGenesis (l1, l2, l2_time, system_config)
#   block_time           — u64
#   max_sequencer_drift  — u64
#   seq_window_size      — u64
#   channel_timeout      — u64
#   granite_channel_timeout — u64 (default 50)
#   l1_chain_id          — u64
#   l2_chain_id          — u64 (MUST be a JSON number, not string)
#   regolith_time..jovian_time — u64 (hardfork timestamps, 0 = active at genesis)
#   batch_inbox_address  — address
#   deposit_contract_address — address
#   l1_system_config_address — address
#   protocol_versions_address — address (zero for devnet)
#   chain_op_config      — FeeConfig {eip1559Elasticity, eip1559Denominator, eip1559DenominatorCanyon}
#
# Intentionally OMITTED (optional fields, skip_serializing_if):
#   blobs_data                — not enabling blobs in devnet
#   pectra_blob_schedule_time — only for Base Sepolia
#   base                      — added by setup-l2.sh post-processing (azul/beryl)

jq -n \
  --arg l1_hash "$L1_GENESIS_HASH" \
  --arg l2_hash "$L2_GENESIS_HASH" \
  --argjson l2_time "$L2_GENESIS_TIME" \
  --arg batcher_addr "$BATCHER_ADDR" \
  --arg overhead "$OVERHEAD_RAW" \
  --arg scalar "$SCALAR_RAW" \
  --argjson gas_limit "$GAS_LIMIT" \
  --argjson block_time "$BLOCK_TIME" \
  --argjson max_seq_drift "$MAX_SEQUENCER_DRIFT" \
  --argjson seq_window "$SEQ_WINDOW_SIZE" \
  --argjson chan_timeout "$CHANNEL_TIMEOUT" \
  --argjson granite_chan_timeout "$GRANITE_CHANNEL_TIMEOUT" \
  --argjson l1_chain_id "$L1_CHAIN_ID" \
  --argjson l2_chain_id "$L2_CHAIN_ID" \
  --arg batch_inbox "$BATCH_INBOX" \
  --arg deposit_contract "$DEPOSIT_CONTRACT" \
  --arg system_config_addr "$SYSTEM_CONFIG_PROXY" \
  '{
    genesis: {
      l1: { hash: $l1_hash, number: 0 },
      l2: { hash: $l2_hash, number: 0 },
      l2_time: $l2_time,
      system_config: {
        batcherAddr: $batcher_addr,
        overhead: $overhead,
        scalar: $scalar,
        gasLimit: $gas_limit
      }
    },
    block_time: $block_time,
    max_sequencer_drift: $max_seq_drift,
    seq_window_size: $seq_window,
    channel_timeout: $chan_timeout,
    granite_channel_timeout: $granite_chan_timeout,
    l1_chain_id: $l1_chain_id,
    l2_chain_id: $l2_chain_id,
    regolith_time: 0,
    canyon_time: 0,
    delta_time: 0,
    ecotone_time: 0,
    fjord_time: 0,
    granite_time: 0,
    holocene_time: 0,
    isthmus_time: 0,
    jovian_time: 0,
    batch_inbox_address: $batch_inbox,
    deposit_contract_address: $deposit_contract,
    l1_system_config_address: $system_config_addr,
    protocol_versions_address: "0x0000000000000000000000000000000000000000",
    chain_op_config: {
      eip1559Elasticity: 6,
      eip1559Denominator: 50,
      eip1559DenominatorCanyon: 250
    }
  }' >"$OUTPUT_DIR/rollup.json"

echo "rollup.json written to $OUTPUT_DIR/rollup.json"
echo ""
echo "=== Rollup Config Assembly Complete ==="
