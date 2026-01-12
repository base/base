#!/usr/bin/env bash
# Launcher script for bundler-spec-tests
# This script is invoked by the test framework with: launcher.sh {start|stop|restart}
#
# Environment variables:
#   ENTRYPOINT_VERSION - v0.6 or v0.7 (required)
#   NODE_PORT          - RPC port (default: 8545)
#   DATA_DIR           - Node data directory (default: /tmp/bundler-spec-test)
#   LOG_DIR            - Log directory (default: ./logs)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../../.." && pwd)"

# Configuration
ENTRYPOINT_VERSION="${ENTRYPOINT_VERSION:-v0.7}"
NODE_PORT="${NODE_PORT:-8545}"
DATA_DIR="${DATA_DIR:-/tmp/bundler-spec-test}"
LOG_DIR="${LOG_DIR:-$SCRIPT_DIR/logs}"
PID_FILE="$LOG_DIR/node.pid"
NODE_LOG="$LOG_DIR/node.log"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

# EntryPoint addresses
ENTRYPOINT_V06="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
ENTRYPOINT_V07="0x0000000071727De22E5E9d8BAf0edAc6f37da032"

get_entrypoint_address() {
    case "$ENTRYPOINT_VERSION" in
        v0.6|0.6)
            echo "$ENTRYPOINT_V06"
            ;;
        v0.7|0.7)
            echo "$ENTRYPOINT_V07"
            ;;
        *)
            echo -e "${RED}Unknown version: $ENTRYPOINT_VERSION${NC}" >&2
            exit 1
            ;;
    esac
}

ensure_binary() {
    local binary="$ROOT_DIR/target/release/base-reth-node"
    if [ ! -f "$binary" ]; then
        echo -e "${BLUE}Building base-reth-node...${NC}"
        cd "$ROOT_DIR"
        cargo build --release -p base-reth-node
    fi
    echo "$binary"
}

wait_for_node() {
    local max_attempts=60
    local attempt=0
    
    echo -n "Waiting for node to be ready"
    while [ $attempt -lt $max_attempts ]; do
        if curl -s -X POST -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
            "http://localhost:$NODE_PORT" > /dev/null 2>&1; then
            echo -e " ${GREEN}ready!${NC}"
            return 0
        fi
        echo -n "."
        sleep 1
        attempt=$((attempt + 1))
    done
    
    echo -e " ${RED}timeout!${NC}"
    return 1
}

deploy_contracts() {
    local version="$1"
    echo -e "${BLUE}Deploying EntryPoint contracts for $version...${NC}"
    
    # Normalize version format
    local deploy_version
    case "$version" in
        v0.6|0.6) deploy_version="0.6" ;;
        v0.7|0.7) deploy_version="0.7" ;;
        *) deploy_version="$version" ;;
    esac
    
    # Use the existing deploy script
    local deploy_script="$ROOT_DIR/crates/account-abstraction/deploy_contracts.sh"
    if [ -f "$deploy_script" ]; then
        RPC_URL="http://localhost:$NODE_PORT" \
        ACCOUNT_INDEX=1 \
            "$deploy_script" "$deploy_version" >> "$LOG_DIR/deploy.log" 2>&1
        echo -e "${GREEN}✓ Contracts deployed${NC}"
    else
        echo -e "${YELLOW}⚠ Deploy script not found, assuming contracts pre-deployed${NC}"
    fi
}

start_node() {
    echo -e "${BLUE}Starting node for $ENTRYPOINT_VERSION tests...${NC}"
    
    mkdir -p "$LOG_DIR"
    
    # Check if already running
    if [ -f "$PID_FILE" ]; then
        local pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo -e "${YELLOW}Node already running (PID: $pid)${NC}"
            return 0
        fi
        rm -f "$PID_FILE"
    fi
    
    # Ensure binary exists
    local binary
    binary=$(ensure_binary)
    
    # Clean data directory for fresh start
    rm -rf "$DATA_DIR"
    mkdir -p "$DATA_DIR"
    
    # Initialize the chain
    echo -e "Initializing chain..."
    "$binary" init --datadir "$DATA_DIR" --chain dev >> "$NODE_LOG" 2>&1
    
    # Start the node in background
    echo -e "Starting node..."
    RUST_LOG=info "$binary" node \
        --datadir "$DATA_DIR" \
        --chain dev \
        --http \
        --http.api eth,net,web3,debug,trace,txpool,rpc,admin \
        --http.addr 0.0.0.0 \
        --http.port "$NODE_PORT" \
        --dev \
        --account-abstraction.enabled \
        --account-abstraction.debug \
        >> "$NODE_LOG" 2>&1 &
    
    local pid=$!
    echo "$pid" > "$PID_FILE"
    
    # Wait for node to be ready
    if ! wait_for_node; then
        echo -e "${RED}Node failed to start. Check logs: $NODE_LOG${NC}"
        stop_node
        exit 1
    fi
    
    # Deploy contracts
    deploy_contracts "$ENTRYPOINT_VERSION"
    
    echo -e "${GREEN}✓ Node started (PID: $pid)${NC}"
    echo -e "  RPC: http://localhost:$NODE_PORT"
    echo -e "  EntryPoint: $(get_entrypoint_address)"
    echo -e "  Logs: $NODE_LOG"
}

stop_node() {
    echo -e "${BLUE}Stopping node...${NC}"
    
    if [ -f "$PID_FILE" ]; then
        local pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            
            # Wait for graceful shutdown
            local attempts=0
            while kill -0 "$pid" 2>/dev/null && [ $attempts -lt 10 ]; do
                sleep 1
                attempts=$((attempts + 1))
            done
            
            # Force kill if still running
            if kill -0 "$pid" 2>/dev/null; then
                kill -9 "$pid" 2>/dev/null || true
            fi
            
            echo -e "${GREEN}✓ Node stopped${NC}"
        else
            echo -e "${YELLOW}Node was not running${NC}"
        fi
        rm -f "$PID_FILE"
    else
        echo -e "${YELLOW}No PID file found${NC}"
        
        # Try to find and kill any orphaned processes
        pkill -f "base-reth-node.*--http.port $NODE_PORT" 2>/dev/null || true
    fi
    
    # Clean up data directory
    rm -rf "$DATA_DIR"
}

restart_node() {
    stop_node
    sleep 2
    start_node
}

case "${1:-}" in
    start)
        start_node
        ;;
    stop)
        stop_node
        ;;
    restart)
        restart_node
        ;;
    *)
        echo "Usage: $0 {start|stop|restart}"
        echo ""
        echo "Environment variables:"
        echo "  ENTRYPOINT_VERSION - v0.6 or v0.7 (default: v0.7)"
        echo "  NODE_PORT          - RPC port (default: 8545)"
        echo "  DATA_DIR           - Node data directory"
        echo "  LOG_DIR            - Log directory"
        exit 1
        ;;
esac
