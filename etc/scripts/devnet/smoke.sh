#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

L1_RPC="${1:-$L1_RPC_URL}"
PK="${2:-$ANVIL_ACCOUNT_1_KEY}"
TO="${3:-$ANVIL_ACCOUNT_2_ADDR}"
L1_BEFORE="$(cast block-number --rpc-url "$L1_RPC")"
L2_BEFORE="$(cast block-number --rpc-url "$L2_CLIENT_RPC_URL")"

echo "=== L1 Transaction Tests ==="
echo "Sending L1 ETH tx..."
cast send --private-key $PK --rpc-url $L1_RPC $TO --value 0.001ether --json | jq -r '"ETH tx: \(.transactionHash) block=\(.blockNumber) status=\(.status)"'

echo "Sending L1 blob tx..."
echo "blob" | cast send --private-key $PK --rpc-url $L1_RPC --blob --path /dev/stdin $TO --json | jq -r '"Blob tx: \(.transactionHash) block=\(.blockNumber) status=\(.status) blobGas=\(.blobGasUsed)"'

echo ""
echo "=== L1 Base Contract Verification ==="
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
echo "=== L1 Portal Deposit Test ==="
PORTAL="$(jq -er '.OptimismPortalProxy' "$ADDRESSES")"
# Unfunded, dedicated test recipient keeps balance arithmetic within shell integers.
DEPOSIT_TO=0x00000000000000000000000000000000deadc0de
BEFORE="$(cast balance --rpc-url "$L2_CLIENT_RPC_URL" "$DEPOSIT_TO")"
cast send --private-key "$PK" --rpc-url "$L1_RPC" "$PORTAL" \
    "depositTransaction(address,uint256,uint64,bool,bytes)" \
    "$DEPOSIT_TO" 1000000000 100000 false 0x --value 1000000000wei --json |
    jq -er 'select(.status == "0x1") | "Deposit: \(.transactionHash) L1 block=\(.blockNumber)"'
for ((attempt = 0; attempt < 180; attempt++)); do
    AFTER="$(cast balance --rpc-url "$L2_CLIENT_RPC_URL" "$DEPOSIT_TO")"
    if (( AFTER == BEFORE + 1000000000 )); then break; fi
    sleep 1
done
(( AFTER == BEFORE + 1000000000 )) || { echo "Deposit did not reach L2" >&2; exit 1; }
echo "Portal deposit credited on L2"
L1_AFTER="$(cast block-number --rpc-url "$L1_RPC")"
L2_AFTER="$(cast block-number --rpc-url "$L2_CLIENT_RPC_URL")"
(( L1_AFTER > L1_BEFORE )) || { echo "L1 head did not advance: $L1_BEFORE -> $L1_AFTER" >&2; exit 1; }
(( L2_AFTER > L2_BEFORE )) || { echo "L2 head did not advance: $L2_BEFORE -> $L2_AFTER" >&2; exit 1; }

echo ""
echo "=== L2 Ingress Transaction Tests ==="
INGRESS_HEALTH_URL="http://localhost:${L2_INGRESS_HEALTH_PORT:-8081}/health"
if curl -sf "$INGRESS_HEALTH_URL" >/dev/null 2>&1; then
    echo "Sending L2 tx through ingress..."
    sleep 3  # wait for the previous tx's nonce to be reflected on-chain
    cast send --private-key $PK --rpc-url $L2_INGRESS_RPC_URL $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'
else
    echo "Ingress not running (start with: just devnet ingress)"
fi
