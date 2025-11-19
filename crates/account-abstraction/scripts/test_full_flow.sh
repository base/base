#!/bin/bash
# Complete end-to-end test: Create and submit a UserOperation bundle
set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8545}"
ENTRYPOINT="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
BUNDLER_KEY="${BUNDLER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
OWNER_KEY="${OWNER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"

echo "═══════════════════════════════════════════════════════"
echo "    UserOperation End-to-End Test"
echo "═══════════════════════════════════════════════════════"
echo ""

# Check if deployment info exists
if [ ! -f "deployment-info.json" ]; then
    echo "❌ deployment-info.json not found!"
    echo "Run ./scripts/deploy_contracts.sh first"
    exit 1
fi

# Load deployment info
WALLET=$(jq -r '.exampleWalletAddress' deployment-info.json)
CHAIN_ID=$(jq -r '.chainId' deployment-info.json)

# Remove leading zeros from wallet address if any
WALLET=$(echo $WALLET | sed 's/0x0*/0x/')

echo "📋 Configuration:"
echo "   RPC: $RPC_URL"
echo "   Chain ID: $CHAIN_ID"
echo "   EntryPoint: $ENTRYPOINT"
echo "   Wallet: $WALLET"
echo "   Bundler: $(cast wallet address $BUNDLER_KEY)"
echo ""

# Check wallet balance
BALANCE=$(cast balance $WALLET --rpc-url $RPC_URL)
echo "💰 Wallet Balance: $(cast to-unit $BALANCE ether) ETH"

if [ "$BALANCE" == "0" ]; then
    echo ""
    echo "⚠️  Wallet has no balance! Funding..."
    cast send $WALLET --value 1ether --private-key $BUNDLER_KEY --rpc-url $RPC_URL > /dev/null
    echo "✅ Funded wallet with 1 ETH"
fi

echo ""
echo "─────────────────────────────────────────────────────────"
echo "🚀 Submitting UserOperation Bundle to EntryPoint"
echo "─────────────────────────────────────────────────────────"
echo ""

# Submit the bundle
cd /Users/ericliu/Projects/node-reth/crates/account-abstraction

cargo run --example submit_bundle -- \
    --bundler-key $BUNDLER_KEY \
    --owner-key $OWNER_KEY \
    --sender $WALLET \
    --entry-point $ENTRYPOINT \
    --chain-id $CHAIN_ID \
    --rpc-url $RPC_URL \
    --nonce 0

echo ""
echo "═══════════════════════════════════════════════════════"
echo "✅ Test Complete!"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "Check your node logs to see the UserOperationEvent!"
echo ""


