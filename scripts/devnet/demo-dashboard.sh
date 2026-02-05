#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

# Configuration
MAX_RETRIES=120  # 120 * 2 seconds = 4 minutes
RETRY_INTERVAL=2
RUN_LOAD=${1:-false}

echo "=== Starting Devnet with Dashboard ==="
echo ""

# Step 1: Start devnet
echo "[1/4] Starting devnet..."
cd "$SCRIPT_DIR/../.."
just devnet

# Step 2: Wait for L2 builder to be healthy
echo ""
echo "[2/4] Waiting for devnet to be healthy..."
RETRY_COUNT=0
while true; do
    if cast block-number --rpc-url "$L2_BUILDER_RPC_URL" &>/dev/null; then
        BLOCK=$(cast block-number --rpc-url "$L2_BUILDER_RPC_URL" 2>/dev/null || echo "0")
        if [ "$BLOCK" -gt 0 ]; then
            echo "  L2 Builder is producing blocks (block #$BLOCK)"
            break
        fi
    fi

    RETRY_COUNT=$((RETRY_COUNT + 1))
    if [ $RETRY_COUNT -ge $MAX_RETRIES ]; then
        echo "ERROR: Devnet failed to reach healthy state after $((MAX_RETRIES * RETRY_INTERVAL)) seconds"
        exit 1
    fi
    echo "  Waiting... ($RETRY_COUNT/$MAX_RETRIES)"
    sleep $RETRY_INTERVAL
done

# Step 3: Run smoke tests
echo ""
echo "[3/4] Running smoke tests..."
./scripts/devnet/smoke.sh

# Step 4: Display dashboard info
echo ""
echo "[4/4] Devnet is ready!"
echo ""
echo "=============================================="
echo "  DASHBOARD:  http://localhost:${L2_CLIENT_DASHBOARD_PORT:-8080}"
echo "=============================================="
echo ""
echo "  L1 RPC:           http://localhost:${L1_HTTP_PORT:-4545}"
echo "  L2 Builder RPC:   http://localhost:${L2_BUILDER_HTTP_PORT:-7545}"
echo "  L2 Client RPC:    http://localhost:${L2_CLIENT_HTTP_PORT:-8545}"
echo ""

# Optional: Start load generator
if [ "$RUN_LOAD" = "true" ] || [ "$RUN_LOAD" = "load" ]; then
    echo "Starting load generator (Contender)..."
    just devnet-load
    echo ""
    echo "Load generator is running. View logs with:"
    echo "  docker logs -f contender"
fi

echo "To send more transactions:"
echo "  just devnet-smoke   # Quick smoke tests"
echo "  just devnet-load    # Start load generator"
echo ""
echo "To stop devnet:"
echo "  just devnet-down"
