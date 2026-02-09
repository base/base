#!/bin/bash
# Sends consistent transactions every 100-200ms to fill gaps between contender bursts

RPC_URL="${RPC_URL:-http://localhost:7545}"
PRIVATE_KEY="${PRIVATE_KEY:-0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6}"
TO_ADDRESS="${TO_ADDRESS:-0x0000000000000000000000000000000000000001}"

echo "Starting consistent transaction sender..."
echo "RPC: $RPC_URL"

while true; do
    # Send a simple ETH transfer with random value (1-100 wei)
    VALUE=$((RANDOM % 100 + 1))

    # Send async (don't wait for receipt)
    cast send --async \
        --rpc-url "$RPC_URL" \
        --private-key "$PRIVATE_KEY" \
        "$TO_ADDRESS" \
        --value "${VALUE}wei" \
        2>/dev/null &

    # Random sleep between 100-200ms
    SLEEP_MS=$((100 + RANDOM % 100))
    sleep "0.${SLEEP_MS}"
done
