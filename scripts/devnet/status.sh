#!/usr/bin/env bash
set -e

# Source .env.devnet if it exists
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$SCRIPT_DIR/../../.env.devnet"
if [ -f "$ENV_FILE" ]; then
    set -a
    source "$ENV_FILE"
    set +a
fi

L1_RPC="${1:-${L1_RPC_URL:-http://localhost:4545}}"
L2_BUILDER_RPC="${2:-${L2_BUILDER_OP_RPC_URL:-http://localhost:7549}}"
L2_BUILDER_OP_RPC="${3:-${L2_BUILDER_OP_RPC_URL:-http://localhost:7549}}"
L2_CLIENT_RPC="${4:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
L2_CLIENT_OP_RPC="${5:-${L2_CLIENT_OP_RPC_URL:-http://localhost:8549}}"

# Fetch L1 block number
L1_BLOCK=$(cast block-number --rpc-url $L1_RPC 2>/dev/null || echo "N/A")

# Fetch L2 builder sync status
BUILDER_STATUS=$(curl -s $L2_BUILDER_OP_RPC -X POST -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"optimism_syncStatus","params":[],"id":1}' 2>/dev/null)
BUILDER_UNSAFE=$(echo $BUILDER_STATUS | jq -r '.result.unsafe_l2.number // "N/A"')
BUILDER_SAFE=$(echo $BUILDER_STATUS | jq -r '.result.safe_l2.number // "N/A"')

# Fetch L2 client sync status
CLIENT_STATUS=$(curl -s $L2_CLIENT_OP_RPC -X POST -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"optimism_syncStatus","params":[],"id":1}' 2>/dev/null)
CLIENT_UNSAFE=$(echo $CLIENT_STATUS | jq -r '.result.unsafe_l2.number // "N/A"')
CLIENT_SAFE=$(echo $CLIENT_STATUS | jq -r '.result.safe_l2.number // "N/A"')

# Print table
printf "\n"
printf "%-12s | %-10s | %-10s\n" "Component" "Unsafe" "Safe"
printf "%-12s-+-%-10s-+-%-10s\n" "------------" "----------" "----------"
printf "%-12s | %-10s | %-10s\n" "L1" "$L1_BLOCK" "-"
printf "%-12s | %-10s | %-10s\n" "L2 Builder" "$BUILDER_UNSAFE" "$BUILDER_SAFE"
printf "%-12s | %-10s | %-10s\n" "L2 Client" "$CLIENT_UNSAFE" "$CLIENT_SAFE"
printf "\n"
