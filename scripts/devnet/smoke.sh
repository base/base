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
PK="${2:-${ANVIL_ACCOUNT_1_KEY:-0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d}}"
TO="${3:-${ANVIL_ACCOUNT_2_ADDR:-0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC}}"

L2_BUILDER_RPC="${L2_BUILDER_RPC_URL:-http://localhost:7545}"
L2_CLIENT_RPC="${L2_CLIENT_RPC_URL:-http://localhost:8545}"

echo "=== L1 Transaction Tests ==="
echo "Sending L1 ETH tx..."
cast send --private-key $PK --rpc-url $L1_RPC $TO --value 0.001ether --json | jq -r '"ETH tx: \(.transactionHash) block=\(.blockNumber) status=\(.status)"'

echo "Sending L1 blob tx..."
echo "blob" | cast send --private-key $PK --rpc-url $L1_RPC --blob --path /dev/stdin $TO --json | jq -r '"Blob tx: \(.transactionHash) block=\(.blockNumber) status=\(.status) blobGas=\(.blobGasUsed)"'

echo ""
echo "=== L1 OP Stack Contract Verification ==="
ADDRESSES=".devnet/l1/configs/l2/l1-addresses.json"
echo "Checking OptimismPortal..." && cast code --rpc-url $L1_RPC $(cat $ADDRESSES | jq -r '.OptimismPortalProxy') | head -c 100 && echo "... (deployed)"
echo "Checking SystemConfig..." && cast code --rpc-url $L1_RPC $(cat $ADDRESSES | jq -r '.SystemConfigProxy') | head -c 100 && echo "... (deployed)"
echo "Checking L1StandardBridge..." && cast code --rpc-url $L1_RPC $(cat $ADDRESSES | jq -r '.L1StandardBridgeProxy') | head -c 100 && echo "... (deployed)"

echo ""
echo "=== L2 Transaction Tests ==="
echo "Sending L2 tx to builder..."
cast send --private-key $PK --rpc-url $L2_BUILDER_RPC $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'

echo "Sending L2 tx to client..."
cast send --private-key $PK --rpc-url $L2_CLIENT_RPC $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'
