#!/usr/bin/env bash
set -euo pipefail

# compute-config-hash.sh — first of two post-genesis one-shots for dev multiproof.
#
# Computes the multiproof CONFIG_HASH from the committed rollup config file and writes it to
# the shared volume for register-aggregate-verifier.sh to consume. Uses `nitro-host
# config-hash`, which reads the rollup config from `rollup.json` and hashes it with the same
# PerChainConfig encoding the enclave uses — so the value is exactly what the on-chain
# AggregateVerifier.CONFIG_HASH must be.
#
# Runs inside the nitro-host-local image (which ships the binary at /app/base-prover-nitro-host).

OUTPUT_DIR="${OUTPUT_DIR:-/configs}"
ROLLUP_CONFIG="${ROLLUP_CONFIG:-$OUTPUT_DIR/rollup.json}"
HASH_FILE="${HASH_FILE:-$OUTPUT_DIR/multiproof-config-hash}"
NITRO_HOST="${NITRO_HOST:-/app/base-prover-nitro-host}"

echo "=== Compute multiproof config hash ==="
echo "Rollup config: $ROLLUP_CONFIG"
echo "Hash file:     $HASH_FILE"

mkdir -p "$OUTPUT_DIR"

# config-hash prints only the hash to stdout; grep guards against any incidental log output.
HASH="$("$NITRO_HOST" config-hash --rollup-config "$ROLLUP_CONFIG" | grep -oiE '0x[0-9a-f]{64}' | tail -n1)"
if ! [[ "$HASH" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
  echo "ERROR: failed to compute a valid config hash (got: '$HASH')"
  exit 1
fi

printf '%s' "$HASH" >"$HASH_FILE"
echo "Wrote config hash $HASH to $HASH_FILE"
