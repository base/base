#!/bin/bash
# Deploy SimpleAccount contracts using cast
#
# Prerequisites:
#   - foundry (forge, cast) installed
#   - RPC node running at RPC_URL
#   - Funded deployer account
#
# Usage:
#   ./scripts/deploy_contracts.sh

set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8545}"
DEPLOYER_PRIVATE_KEY="${DEPLOYER_PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"  # Default anvil key
ENTRYPOINT_ADDRESS="${ENTRYPOINT_ADDRESS:-0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789}"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}    EIP-4337 SimpleAccount Contract Deployment${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Check if cast is installed
if ! command -v cast &> /dev/null; then
    echo -e "${RED}❌ Error: cast (foundry) is not installed${NC}"
    echo -e "${YELLOW}Install from: https://book.getfoundry.sh/getting-started/installation${NC}"
    exit 1
fi

# Check if solc is installed
if ! command -v solc &> /dev/null; then
    echo -e "${RED}❌ Error: solc is not installed${NC}"
    echo -e "${YELLOW}Install from: https://docs.soliditylang.org/en/latest/installing-solidity.html${NC}"
    exit 1
fi

echo -e "${YELLOW}📋 Configuration:${NC}"
echo -e "   RPC URL: $RPC_URL"
echo -e "   EntryPoint: $ENTRYPOINT_ADDRESS"
echo -e "   Deployer: $(cast wallet address $DEPLOYER_PRIVATE_KEY)"
echo ""

# Get deployer balance
BALANCE=$(cast balance $(cast wallet address $DEPLOYER_PRIVATE_KEY) --rpc-url $RPC_URL)
echo -e "${YELLOW}💰 Deployer Balance: $(cast to-unit $BALANCE ether) ETH${NC}"
echo ""

# Change to contracts directory
cd "$(dirname "$0")/../contracts"

echo -e "${YELLOW}[1/3] Compiling contracts...${NC}"
# Compile IEntryPoint
solc --optimize --optimize-runs 200 --bin --abi IEntryPoint.sol -o build/ --overwrite
# Compile SimpleAccount
solc --optimize --optimize-runs 200 --bin --abi SimpleAccount.sol -o build/ --overwrite --allow-paths .
# Compile SimpleAccountFactory
solc --optimize --optimize-runs 200 --bin --abi SimpleAccountFactory.sol -o build/ --overwrite --allow-paths .
echo -e "${GREEN}✓ Contracts compiled${NC}"
echo ""

echo -e "${YELLOW}[2/3] Deploying SimpleAccountFactory...${NC}"

# Read the compiled bytecode
FACTORY_BYTECODE=$(cat build/SimpleAccountFactory.bin)

# Encode constructor arguments (entryPoint address)
CONSTRUCTOR_ARGS=$(cast abi-encode "constructor(address)" $ENTRYPOINT_ADDRESS)

# Combine bytecode and constructor
DEPLOY_CODE="${FACTORY_BYTECODE}${CONSTRUCTOR_ARGS:2}"

# Deploy the factory
FACTORY_ADDRESS=$(cast send --private-key $DEPLOYER_PRIVATE_KEY \
    --rpc-url $RPC_URL \
    --create $DEPLOY_CODE \
    --json | jq -r '.contractAddress')

if [ -z "$FACTORY_ADDRESS" ] || [ "$FACTORY_ADDRESS" == "null" ]; then
    echo -e "${RED}❌ Failed to deploy SimpleAccountFactory${NC}"
    exit 1
fi

echo -e "${GREEN}✓ SimpleAccountFactory deployed at: $FACTORY_ADDRESS${NC}"
echo ""

echo -e "${YELLOW}[3/3] Computing wallet address...${NC}"

# Example: Compute the address of a SimpleAccount for owner and salt
OWNER_ADDRESS="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"  # Default anvil address #0
SALT="0"

# Get the address without deploying
WALLET_ADDRESS=$(cast call $FACTORY_ADDRESS \
    "getAddress(address,uint256)" \
    $OWNER_ADDRESS \
    $SALT \
    --rpc-url $RPC_URL)

echo -e "${GREEN}✓ Wallet address (owner=$OWNER_ADDRESS, salt=$SALT): $WALLET_ADDRESS${NC}"
echo ""

# Save deployment info
DEPLOYMENT_FILE="../deployment-info.json"
cat > $DEPLOYMENT_FILE << EOF
{
  "entryPoint": "$ENTRYPOINT_ADDRESS",
  "simpleAccountFactory": "$FACTORY_ADDRESS",
  "exampleWalletAddress": "$WALLET_ADDRESS",
  "exampleOwner": "$OWNER_ADDRESS",
  "exampleSalt": "$SALT",
  "rpcUrl": "$RPC_URL",
  "chainId": "$(cast chain-id --rpc-url $RPC_URL)"
}
EOF

echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}✅ Deployment Complete!${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""
echo -e "${YELLOW}📄 Deployment info saved to: $DEPLOYMENT_FILE${NC}"
echo ""
echo -e "${YELLOW}🚀 Next steps:${NC}"
echo ""
echo -e "1. Fund the wallet with ETH for gas:"
echo -e "   ${BLUE}cast send $WALLET_ADDRESS --value 0.1ether --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL${NC}"
echo ""
echo -e "2. Create a UserOperation:"
echo -e "   ${BLUE}cargo run --example create_userop -- \\${NC}"
echo -e "   ${BLUE}  --private-key ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \\${NC}"
echo -e "   ${BLUE}  --sender $WALLET_ADDRESS \\${NC}"
echo -e "   ${BLUE}  --entry-point $ENTRYPOINT_ADDRESS \\${NC}"
echo -e "   ${BLUE}  --chain-id $(cast chain-id --rpc-url $RPC_URL)${NC}"
echo ""


