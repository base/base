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
echo "=== L2 Ingress Transaction Tests ==="
INGRESS_HEALTH_URL="http://localhost:${L2_INGRESS_HEALTH_PORT:-8081}/health"
if curl -sf "$INGRESS_HEALTH_URL" >/dev/null 2>&1; then
    echo "Sending L2 tx through ingress..."
    sleep 3  # wait for the previous tx's nonce to be reflected on-chain
    cast send --private-key $PK --rpc-url $L2_INGRESS_RPC_URL $TO --value 0.001ether --json | jq -r '"TX: \(.transactionHash) block=\(.blockNumber)"'
else
    echo "Ingress not running (start with: just devnet ingress)"
fi

# === Multiproof AggregateVerifier Registration (L3 dev-multiproof only) ===
# The config-hash file is written by the compute-config-hash one-shot in the L3 devnet; its
# presence scopes this assertion to dev-multiproof runs. Verify the AggregateVerifier registered
# for the multiproof game type carries the runtime-computed CONFIG_HASH (deferred registration).
CONFIG_HASH_FILE=".devnet/l2/configs/multiproof-config-hash"
if [ -f "$CONFIG_HASH_FILE" ]; then
    echo ""
    echo "=== Multiproof AggregateVerifier Verification ==="
    MULTIPROOF_GAME_TYPE="${MULTIPROOF_GAME_TYPE:-621}"
    EXPECTED_HASH=$(tr -d '[:space:]' <"$CONFIG_HASH_FILE" | tr '[:upper:]' '[:lower:]')
    DGF=$(jq -r '.DisputeGameFactoryProxy' "$ADDRESSES")

    VERIFIER=$(cast call --rpc-url "$L1_RPC" "$DGF" "gameImpls(uint32)(address)" "$MULTIPROOF_GAME_TYPE")
    if [ -z "$VERIFIER" ] || [ "$VERIFIER" = "0x0000000000000000000000000000000000000000" ]; then
        echo "FAIL: no AggregateVerifier registered for game type $MULTIPROOF_GAME_TYPE"
        exit 1
    fi
    echo "AggregateVerifier (game type $MULTIPROOF_GAME_TYPE): $VERIFIER"

    ONCHAIN_HASH=$(cast call --rpc-url "$L1_RPC" "$VERIFIER" "CONFIG_HASH()(bytes32)" | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')
    echo "On-chain CONFIG_HASH: $ONCHAIN_HASH"
    echo "Expected  CONFIG_HASH: $EXPECTED_HASH"

    if [ "$ONCHAIN_HASH" != "$EXPECTED_HASH" ]; then
        echo "FAIL: AggregateVerifier CONFIG_HASH does not match the computed config hash"
        exit 1
    fi
    echo "PASS: AggregateVerifier CONFIG_HASH matches the runtime-computed config hash"
fi
