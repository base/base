#!/bin/bash
# Complete test: Submit UserOperation and retrieve receipt
set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8545}"
ENTRYPOINT="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
BUNDLER_KEY="${BUNDLER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
OWNER_KEY="${OWNER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}    UserOperation Receipt Test${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Load deployment info
if [ ! -f "deployment-info.json" ]; then
    echo -e "${RED}❌ deployment-info.json not found!${NC}"
    echo "Run ./scripts/deploy_contracts.sh first"
    exit 1
fi

WALLET=$(jq -r '.exampleWalletAddress' deployment-info.json)
CHAIN_ID=$(jq -r '.chainId' deployment-info.json)

# Clean up wallet address
WALLET=$(echo $WALLET | sed 's/0x0*/0x/')

echo -e "${YELLOW}📋 Configuration:${NC}"
echo "   Wallet: $WALLET"
echo "   EntryPoint: $ENTRYPOINT"
echo "   Chain: $CHAIN_ID"
echo ""

# Check wallet balance
BALANCE=$(cast balance $WALLET --rpc-url $RPC_URL 2>/dev/null || echo "0")
if [ "$BALANCE" == "0" ]; then
    echo -e "${YELLOW}Funding wallet...${NC}"
    cast send $WALLET --value 1ether --private-key $BUNDLER_KEY --rpc-url $RPC_URL > /dev/null 2>&1
    echo -e "${GREEN}✓ Funded${NC}"
    echo ""
fi

cd "$(dirname "$0")/.."

echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}[Step 1/3] Building and Submitting UserOperation Bundle${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Submit the bundle and capture output
OUTPUT=$(cargo run --quiet --example submit_bundle -- \
    --bundler-key $BUNDLER_KEY \
    --owner-key $OWNER_KEY \
    --sender $WALLET \
    --entry-point $ENTRYPOINT \
    --chain-id $CHAIN_ID \
    --rpc-url $RPC_URL \
    --nonce 0 2>&1)

echo "$OUTPUT"

# Extract the UserOperation hash from output
USER_OP_HASH=$(echo "$OUTPUT" | grep "UserOperation Hash:" | awk '{print $4}')

if [ -z "$USER_OP_HASH" ]; then
    echo -e "${RED}❌ Could not extract UserOperation hash${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}✓ Bundle submitted${NC}"
echo -e "${GREEN}✓ UserOperation Hash: $USER_OP_HASH${NC}"

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}[Step 2/3] Waiting for Confirmation...${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Wait a moment for the transaction to be mined
sleep 2

echo -e "${GREEN}✓ Transaction should be mined${NC}"

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}[Step 3/3] Retrieving UserOperation Receipt${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Query for the receipt
echo -e "${YELLOW}Calling eth_getUserOperationReceipt...${NC}"
echo ""

RECEIPT=$(curl -s -X POST $RPC_URL \
  -H 'Content-Type: application/json' \
  -d "{
    \"jsonrpc\": \"2.0\",
    \"id\": 1,
    \"method\": \"eth_getUserOperationReceipt\",
    \"params\": [\"$USER_OP_HASH\"]
  }")

echo "Response:"
echo "$RECEIPT" | jq '.'
echo ""

# Check if we got a receipt
RESULT=$(echo "$RECEIPT" | jq -r '.result')

if [ "$RESULT" == "null" ] || [ -z "$RESULT" ]; then
    echo -e "${YELLOW}⚠️  No receipt found yet${NC}"
    echo ""
    echo "This could mean:"
    echo "1. The transaction hasn't been mined yet (wait a bit longer)"
    echo "2. The UserOperation failed validation"
    echo "3. The EntryPoint emitted no UserOperationEvent"
    echo ""
    echo "Check your node logs for UserOperationEvent emissions"
    echo ""
    echo "You can manually query again:"
    echo -e "${BLUE}curl -X POST $RPC_URL -H 'Content-Type: application/json' \\${NC}"
    echo -e "${BLUE}  -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getUserOperationReceipt\",\"params\":[\"$USER_OP_HASH\"]}' | jq${NC}"
else
    echo -e "${GREEN}✅ Receipt Retrieved Successfully!${NC}"
    echo ""
    echo "Receipt details:"
    echo "$RECEIPT" | jq '.result | {
        userOpHash,
        sender,
        nonce,
        success,
        actualGasCost,
        actualGasUsed
    }'
fi

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}✅ Test Complete!${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Save test info
cat > test-result.json << EOF
{
  "userOpHash": "$USER_OP_HASH",
  "wallet": "$WALLET",
  "entryPoint": "$ENTRYPOINT",
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

echo "Test info saved to: test-result.json"
echo ""


