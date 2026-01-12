#!/bin/bash
# Start a fresh local dev node with Account Abstraction enabled

set -e

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo -e "${BLUE}🔨 Building base-reth-node...${NC}"
cargo build --release -p base-reth-node

DATA_DIR="/tmp/reth-aa-dev"
AA_SEND_URL="${AA_SEND_URL:-http://localhost:8080}"

# Check if data directory exists
if [ -d "$DATA_DIR" ]; then
    echo -e "${YELLOW}⚠️  Data directory $DATA_DIR already exists${NC}"
    read -p "Delete and start fresh? (y/n) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        echo -e "${BLUE}🗑️  Removing old data directory...${NC}"
        rm -rf "$DATA_DIR"
    fi
fi

# Initialize if needed
if [ ! -d "$DATA_DIR" ]; then
    echo -e "${BLUE}🎬 Initializing fresh dev chain...${NC}"
    ./target/release/base-reth-node init \
        --datadir "$DATA_DIR" \
        --chain dev
fi

echo -e "${GREEN}✅ Starting node with Account Abstraction enabled${NC}"
echo -e "${GREEN}   RPC: http://localhost:8545${NC}"
echo -e "${GREEN}   Send URL: $AA_SEND_URL${NC}"
echo -e "${GREEN}   Available endpoints:${NC}"
echo -e "${GREEN}     - eth_sendUserOperation${NC}"
echo -e "${GREEN}     - eth_estimateUserOperationGas${NC}"
echo -e "${GREEN}     - eth_getUserOperationByHash${NC}"
echo -e "${GREEN}     - eth_getUserOperationReceipt${NC}"
echo -e "${GREEN}     - eth_supportedEntryPoints${NC}"
echo -e "${GREEN}     - base_validateUserOperation${NC}"
echo ""
echo -e "${YELLOW}Press Ctrl+C to stop${NC}"
echo ""

RUST_LOG=debug ./target/release/base-reth-node node \
    --datadir "$DATA_DIR" \
    --chain dev \
    --http \
    --http.api eth,net,web3,debug,trace,txpool,rpc,admin \
    --http.addr 0.0.0.0 \
    --http.port 8545 \
    --dev \
    --account-abstraction.enabled \
    --account-abstraction.send-url "$AA_SEND_URL" \
    --account-abstraction.debug \
    --account-abstraction.indexer
