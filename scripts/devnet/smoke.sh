#!/usr/bin/env bash
set -e

L1_RPC="${1:-http://localhost:4545}"
PK="${2:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
TO="${3:-0x70997970C51812dc3A010C7d01b50e0d17dc79C8}"

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
cast send --private-key $PK --rpc-url http://localhost:7545 $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'

echo "Sending L2 tx to client..."
cast send --private-key $PK --rpc-url http://localhost:8545 $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'
