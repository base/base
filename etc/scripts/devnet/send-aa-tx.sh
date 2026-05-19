#!/bin/bash
# Runs the maintained EIP-8130 devnet sender script in probe mode.
#
# Usage:
#   ./etc/scripts/devnet/send-aa-tx.sh
set -euo pipefail

L2_RPC="${L2_BUILDER_RPC_URL:-http://localhost:7545}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# Prefer a funded nonce-0 account for probe mode.
SENDER_KEY="${SENDER_KEY:-${ANVIL_ACCOUNT_2_KEY:-0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a}}"
# Use a unique nonce lane by default so probe reruns don't collide.
AA_NONCE_KEY="${AA_NONCE_KEY:-$(( ($(date +%s) % 1000000) + 1 ))}"

echo "=== EIP-8130 AA Probe ==="
echo "RPC: $L2_RPC"
echo "Nonce key: $AA_NONCE_KEY"

if ! command -v node >/dev/null 2>&1; then
  echo "ERROR: node is required."
  exit 1
fi

if ! command -v cast >/dev/null 2>&1; then
  echo "ERROR: cast is required."
  exit 1
fi

BLOCK="$(cast block-number --rpc-url "$L2_RPC" 2>/dev/null || true)"
if [ -z "$BLOCK" ]; then
  echo "ERROR: cannot reach L2 RPC at $L2_RPC"
  exit 1
fi
echo "Current L2 block: $BLOCK"

# Ensure dependencies for send-aa-tx.mjs are present.
if [ ! -d "$SCRIPT_DIR/node_modules/viem" ]; then
  echo "Installing script dependencies..."
  npm install --prefix "$SCRIPT_DIR" viem @noble/curves@1.8.2 >/dev/null
fi

echo "Running probe mode..."
SENDER_KEY="$SENDER_KEY" AA_NONCE_KEY="$AA_NONCE_KEY" \
  node "$SCRIPT_DIR/send-aa-tx.mjs" probe --rpc "$L2_RPC" --nonce-key "$AA_NONCE_KEY" --no-trace
