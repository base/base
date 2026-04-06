#!/usr/bin/env bash
# Test script for the nitro prover local server.
#
# Fetches real L1/L2 data from RPCs and sends a `prover_prove` request
# to the locally running nitro prover server (started via `just tee nitro-local`).
#
# Uses `optimism_outputAtBlock` on the OP rollup node to get canonical output
# roots and L2 block references, rather than recomputing them manually.
#
# Usage:
#   ./etc/scripts/local/test-nitro-prover.sh
#
# Environment variables (defaults match `just tee nitro-local`):
#   OP_NODE_URL     - OP rollup node RPC         (default: base mainnet reth proofs)
#   L1_ETH_URL      - L1 execution RPC           (default: https://ethereum-rpc.publicnode.com)
#   PROVER_RPC_URL  - Prover JSON-RPC endpoint   (default: http://localhost:8000)

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
OP_NODE_URL="${OP_NODE_URL:?must set OP_NODE_URL}"
L1_ETH_URL="${L1_ETH_URL:?must set L1_ETH_URL}"
PROVER_RPC_URL="${PROVER_RPC_URL:-http://localhost:8000}"
BLOCK_RANGE="${BLOCK_RANGE:-10}"
L1_HEAD_BUFFER=50

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
rpc() {
    local url="$1" method="$2" params="$3"
    curl -sf -X POST -H "Content-Type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}" \
        "$url" | jq -r '.result'
}

hex() {
    printf "0x%x" "$1"
}

# ---------------------------------------------------------------------------
# Step 1: Determine the L2 block range to prove
#   claimed = safe head (already fully derived, so all L1 batch data exists)
#   agreed  = safe head - 10 (the prover re-derives 10 blocks)
# ---------------------------------------------------------------------------
sync_status=$(rpc "$OP_NODE_URL" "optimism_syncStatus" "[]")
safe_l2_number=$(echo "$sync_status" | jq -r '.safe_l2.number')
CLAIMED_L2_BLOCK_NUMBER=$((safe_l2_number))
AGREED_L2_BLOCK_NUMBER=$((CLAIMED_L2_BLOCK_NUMBER - BLOCK_RANGE))
echo ""
echo "=== Proving L2 blocks $AGREED_L2_BLOCK_NUMBER → $CLAIMED_L2_BLOCK_NUMBER ($BLOCK_RANGE blocks) ==="
echo "    Agreed L2 block: $AGREED_L2_BLOCK_NUMBER"
echo "    Claimed L2 block (safe head): $CLAIMED_L2_BLOCK_NUMBER"

# ---------------------------------------------------------------------------
# Step 2: Fetch agreed output via optimism_outputAtBlock
# ---------------------------------------------------------------------------
echo ""
echo "--- Fetching agreed L2 output at block $AGREED_L2_BLOCK_NUMBER ---"
agreed_output=$(rpc "$OP_NODE_URL" "optimism_outputAtBlock" "[\"$(hex "$AGREED_L2_BLOCK_NUMBER")\"]")
AGREED_L2_OUTPUT_ROOT=$(echo "$agreed_output" | jq -r '.outputRoot')
AGREED_L2_HEAD_HASH=$(echo "$agreed_output" | jq -r '.blockRef.hash')
agreed_state_root=$(echo "$agreed_output" | jq -r '.stateRoot')
agreed_withdrawal_root=$(echo "$agreed_output" | jq -r '.withdrawalStorageRoot')
agreed_l1_origin_number=$(echo "$agreed_output" | jq -r '.blockRef.l1origin.number')
echo "  blockRef.hash:          $AGREED_L2_HEAD_HASH"
echo "  stateRoot:              $agreed_state_root"
echo "  withdrawalStorageRoot:  $agreed_withdrawal_root"
echo "  outputRoot:             $AGREED_L2_OUTPUT_ROOT"
echo "  blockRef.l1origin:      $agreed_l1_origin_number"

# ---------------------------------------------------------------------------
# Step 3: Fetch claimed output via optimism_outputAtBlock
# ---------------------------------------------------------------------------
echo ""
echo "--- Fetching claimed L2 output at block $CLAIMED_L2_BLOCK_NUMBER ---"
claimed_output=$(rpc "$OP_NODE_URL" "optimism_outputAtBlock" "[\"$(hex "$CLAIMED_L2_BLOCK_NUMBER")\"]")
CLAIMED_L2_OUTPUT_ROOT=$(echo "$claimed_output" | jq -r '.outputRoot')
claimed_block_hash=$(echo "$claimed_output" | jq -r '.blockRef.hash')
claimed_state_root=$(echo "$claimed_output" | jq -r '.stateRoot')
claimed_withdrawal_root=$(echo "$claimed_output" | jq -r '.withdrawalStorageRoot')
claimed_l1_origin_number=$(echo "$claimed_output" | jq -r '.blockRef.l1origin.number')
echo "  blockRef.hash:          $claimed_block_hash"
echo "  stateRoot:              $claimed_state_root"
echo "  withdrawalStorageRoot:  $claimed_withdrawal_root"
echo "  outputRoot:             $CLAIMED_L2_OUTPUT_ROOT"
echo "  blockRef.l1origin:      $claimed_l1_origin_number"

# ---------------------------------------------------------------------------
# Step 4: Find L1 head
#   Use claimed_l1_origin + L1_HEAD_BUFFER, but never exceed the current L1 tip.
#   On mainnet the batcher posts every few L1 blocks, so a small buffer past
#   the claimed L1 origin is sufficient.  The prover walks backwards from
#   l1_head via parent hashes, so keeping the gap small avoids thousands of
#   unnecessary header fetches.
# ---------------------------------------------------------------------------
echo ""
echo "--- Determining L1 head ---"

CURRENT_L1_TIP=$(rpc "$L1_ETH_URL" "eth_blockNumber" "[]")
CURRENT_L1_TIP_DEC=$(printf "%d" "$CURRENT_L1_TIP")
DESIRED_L1_HEAD=$((claimed_l1_origin_number + L1_HEAD_BUFFER))

if [ "$DESIRED_L1_HEAD" -gt "$CURRENT_L1_TIP_DEC" ]; then
    echo "  WARNING: desired L1 head ($DESIRED_L1_HEAD) exceeds current L1 tip ($CURRENT_L1_TIP_DEC)"
    echo "           capping to current L1 tip"
    L1_HEAD_NUMBER=$CURRENT_L1_TIP_DEC
else
    L1_HEAD_NUMBER=$DESIRED_L1_HEAD
fi

L1_HEAD=$(rpc "$L1_ETH_URL" "eth_getBlockByNumber" "[\"$(hex "$L1_HEAD_NUMBER")\", false]" | jq -r '.hash')
if [ "$L1_HEAD" = "null" ] || [ -z "$L1_HEAD" ]; then
    echo "  ERROR: could not fetch L1 block hash at $L1_HEAD_NUMBER" >&2
    exit 1
fi
echo "  agreed L1 origin:    $agreed_l1_origin_number"
echo "  claimed L1 origin:   $claimed_l1_origin_number"
echo "  current L1 tip:      $CURRENT_L1_TIP_DEC"
echo "  L1 head:             $L1_HEAD_NUMBER  (min(claimed l1origin + $L1_HEAD_BUFFER, tip))"
echo "  L1 head hash:        $L1_HEAD"

# ---------------------------------------------------------------------------
# Step 5: Send the proof request
# ---------------------------------------------------------------------------
echo ""
echo "============================================="
echo "  Sending prover_prove request"
echo "============================================="
echo ""
echo "Request payload:"
payload=$(cat <<EOF
{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "prover_prove",
    "params": [{
        "l1_head": "$L1_HEAD",
        "agreed_l2_head_hash": "$AGREED_L2_HEAD_HASH",
        "agreed_l2_output_root": "$AGREED_L2_OUTPUT_ROOT",
        "claimed_l2_output_root": "$CLAIMED_L2_OUTPUT_ROOT",
        "claimed_l2_block_number": $CLAIMED_L2_BLOCK_NUMBER,
        "intermediate_block_interval": $BLOCK_RANGE,
        "l1_head_number": $L1_HEAD_NUMBER
    }]
}
EOF
)
echo "$payload" | jq .

echo "  agreed_l2_block_number:  $AGREED_L2_BLOCK_NUMBER  (l1_origin: $agreed_l1_origin_number)"
echo "  claimed_l2_block_number: $CLAIMED_L2_BLOCK_NUMBER  (l1_origin: $claimed_l1_origin_number)"
echo "  l1_head_block_number:    $L1_HEAD_NUMBER"
echo ""
echo "Sending to $PROVER_RPC_URL ..."
echo "(this may take a while — proof generation is compute-intensive)"
echo ""

response=$(curl -sf -X POST -H "Content-Type: application/json" \
    --data "$payload" \
    --max-time 1800 \
    "$PROVER_RPC_URL")

echo "Response:"
echo "$response" | jq .
