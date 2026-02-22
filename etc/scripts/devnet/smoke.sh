#!/usr/bin/env bash
set -e

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

L1_RPC="${1:-$L1_RPC_URL}"
PK="${2:-$ANVIL_ACCOUNT_1_KEY}"
TO="${3:-$ANVIL_ACCOUNT_2_ADDR}"

echo "=== L1 Transaction Tests ==="
echo "Sending L1 ETH tx..."
cast send --private-key $PK --rpc-url $L1_RPC $TO --value 0.001ether --json | jq -r '"ETH tx: \(.transactionHash) block=\(.blockNumber) status=\(.status)"'

echo "Sending L1 blob tx..."
echo "blob" | cast send --private-key $PK --rpc-url $L1_RPC --blob --path /dev/stdin $TO --json | jq -r '"Blob tx: \(.transactionHash) block=\(.blockNumber) status=\(.status) blobGas=\(.blobGasUsed)"'

echo ""
echo "=== L1 OP Stack Contract Verification ==="
ADDRESSES=".devnet/l2/configs/l1-addresses.json"
echo "Checking OptimismPortal..." && cast code --rpc-url $L1_RPC $(cat $ADDRESSES | jq -r '.OptimismPortalProxy') | head -c 100 && echo "... (deployed)"
echo "Checking SystemConfig..." && cast code --rpc-url $L1_RPC $(cat $ADDRESSES | jq -r '.SystemConfigProxy') | head -c 100 && echo "... (deployed)"
echo "Checking L1StandardBridge..." && cast code --rpc-url $L1_RPC $(cat $ADDRESSES | jq -r '.L1StandardBridgeProxy') | head -c 100 && echo "... (deployed)"

echo ""
echo "=== L2 Transaction Tests ==="
echo "Sending L2 tx to builder..."
cast send --private-key $PK --rpc-url $L2_BUILDER_RPC_URL $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'

echo "Sending L2 tx to client..."
cast send --private-key $PK --rpc-url $L2_CLIENT_RPC_URL $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'

echo ""
echo "=== L2 Ingress Transaction Tests ==="
INGRESS_HEALTH_URL="http://localhost:${L2_INGRESS_HEALTH_PORT:-8081}/health"
if curl -sf "$INGRESS_HEALTH_URL" >/dev/null 2>&1; then
    # Ingress only implements eth_sendRawTransaction — build the tx against
    # the builder (for nonce/chainId) then send the raw bytes through ingress.
    echo "Sending L2 tx through ingress..."
    sleep 3  # wait for the previous tx's nonce to be reflected on-chain
    RAW_TX=$(cast mktx --private-key $PK --rpc-url $L2_BUILDER_RPC_URL $TO --value 0.001ether)
    curl -sf $L2_INGRESS_RPC_URL -X POST -H "Content-Type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendRawTransaction\",\"params\":[\"$RAW_TX\"],\"id\":1}" | jq -r '"TX: \(.result)"'
else
    echo "Ingress not running (start with: just devnet-ingress)"
fi
