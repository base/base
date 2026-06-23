#!/bin/bash
set -euo pipefail

# register-aggregate-verifier.sh — second of two post-genesis one-shots for dev multiproof.
#
# The AggregateVerifier's CONFIG_HASH is immutable and depends on the L2 rollup config, which
# only exists after L2 genesis. So SystemDeploy defers it (MULTIPROOF_DEFER_REGISTRATION=true)
# and this script finishes the job once the hash is known:
#   1. compute-config-hash.sh has written the real hash to $HASH_FILE.
#   2. setup-l2.sh has copied the deploy outfile to the shared volume.
# We regenerate the deploy config (identical to the deploy) and call SystemDeploy's
# registerAggregateVerifier(bytes32) re-entrant entrypoint, which deploys the verifier with the
# real hash and points the DisputeGameFactory's game type at it.

L1_RPC_URL="${L1_RPC_URL:-http://l1-el:4545}"
L2_RPC_URL="${L2_RPC_URL:-http://base-rpc:8645}"
L1_CHAIN_ID="${L1_CHAIN_ID:-1337}"
OUTPUT_DIR="${OUTPUT_DIR:-/devnet/l2/configs}"
TEMPLATE_DIR="${TEMPLATE_DIR:-/templates}"
WORKDIR=/contracts
HASH_FILE="${HASH_FILE:-$OUTPUT_DIR/multiproof-config-hash}"
DEPLOY_OUTFILE="$OUTPUT_DIR/${L1_CHAIN_ID}-deploy.json"
# L2ToL1MessagePasser predeploy; its storage root is part of the OP output-root preimage.
L2_TO_L1_MESSAGE_PASSER=0x4200000000000000000000000000000000000016
# foundry.toml only grants fs_permissions write access to paths under /contracts
# (./deployments/). DEPLOYMENT_OUTFILE must therefore live there, not on the shared volume;
# we stage the shared copy in and copy the result back out (mirrors setup-l2.sh).
LOCAL_DEPLOY_OUTFILE="$WORKDIR/deployments/${L1_CHAIN_ID}-deploy.json"

: "${DEPLOYER_ADDR:?DEPLOYER_ADDR is required}"
: "${DEPLOYER_KEY:?DEPLOYER_KEY is required}"

echo "=== Register AggregateVerifier (post-genesis) ==="
echo "L1 RPC URL:    $L1_RPC_URL"
echo "L1 Chain ID:   $L1_CHAIN_ID"
echo "Hash file:     $HASH_FILE"
echo "Deploy outfile: $DEPLOY_OUTFILE"

# The config hash must be present and well-formed (0x + 64 hex chars). compose ordering
# guarantees compute-config-hash.sh completed first, but validate defensively.
if [ ! -s "$HASH_FILE" ]; then
  echo "ERROR: config hash file missing or empty: $HASH_FILE"
  exit 1
fi
MULTIPROOF_CONFIG_HASH_COMPUTED="$(tr -d '[:space:]' <"$HASH_FILE")"
if ! [[ "$MULTIPROOF_CONFIG_HASH_COMPUTED" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
  echo "ERROR: malformed config hash in $HASH_FILE: '$MULTIPROOF_CONFIG_HASH_COMPUTED'"
  exit 1
fi
echo "Computed config hash: $MULTIPROOF_CONFIG_HASH_COMPUTED"

if [ ! -f "$DEPLOY_OUTFILE" ]; then
  echo "ERROR: deploy outfile not found: $DEPLOY_OUTFILE (setup-l2 must run with MULTIPROOF_DEFER_REGISTRATION=true)"
  exit 1
fi

# Compute the real L2 genesis output root now that L2 genesis exists. The main deploy seeded the
# AnchorStateRegistry with a placeholder (the real root is unknowable pre-genesis), so SystemDeploy
# deferred ASR initialization to registerAggregateVerifier, which reads this value from cfg. Derive
# it from the L2 EL (op-node is being deprecated) using the standard OP output-root formula:
#   keccak256(version=0x0 ++ stateRoot ++ messagePasserStorageRoot ++ blockHash) at block 0.
echo ""
echo "--- Computing L2 genesis output root ---"
echo "L2 RPC URL: $L2_RPC_URL"
MAX_RETRIES=100
RETRY_COUNT=0
until cast block 0 --json --rpc-url "$L2_RPC_URL" >/dev/null 2>&1; do
  RETRY_COUNT=$((RETRY_COUNT + 1))
  if [ "$RETRY_COUNT" -ge "$MAX_RETRIES" ]; then
    echo "ERROR: L2 RPC not ready after $MAX_RETRIES retries: $L2_RPC_URL"
    exit 1
  fi
  sleep 0.5
done

L2_GENESIS_BLOCK="$(cast block 0 --json --rpc-url "$L2_RPC_URL")"
L2_STATE_ROOT="$(echo "$L2_GENESIS_BLOCK" | jq -r '.stateRoot')"
L2_BLOCK_HASH="$(echo "$L2_GENESIS_BLOCK" | jq -r '.hash')"
L2_MESSAGE_PASSER_ROOT="$(cast proof "$L2_TO_L1_MESSAGE_PASSER" --block 0 --rpc-url "$L2_RPC_URL" | jq -r '.storageHash')"
for field in "$L2_STATE_ROOT" "$L2_BLOCK_HASH" "$L2_MESSAGE_PASSER_ROOT"; do
  if ! [[ "$field" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
    echo "ERROR: malformed L2 genesis field while computing output root: '$field'"
    exit 1
  fi
done
OUTPUT_ROOT_VERSION="0x0000000000000000000000000000000000000000000000000000000000000000"
OUTPUT_ROOT_PREIMAGE="0x${OUTPUT_ROOT_VERSION#0x}${L2_STATE_ROOT#0x}${L2_MESSAGE_PASSER_ROOT#0x}${L2_BLOCK_HASH#0x}"
MULTIPROOF_GENESIS_OUTPUT_ROOT="$(cast keccak "$OUTPUT_ROOT_PREIMAGE")"
echo "L2 genesis state root:           $L2_STATE_ROOT"
echo "L2 message passer storage root:  $L2_MESSAGE_PASSER_ROOT"
echo "L2 genesis block hash:           $L2_BLOCK_HASH"
echo "Computed L2 genesis output root: $MULTIPROOF_GENESIS_OUTPUT_ROOT"

# Regenerate the deploy config from the template exactly as setup-l2.sh step 1 did, so the
# verifier's non-hash inputs (teeImageHash, intervals, game type, …) match the original deploy.
# Override MULTIPROOF_CONFIG_HASH with the computed hash (the env carries only the deploy-time
# placeholder) so the regenerated devnet.json is self-consistent with the value we register —
# the forge arg below is authoritative, but this avoids a stale hash on disk being read silently.
# Also inject the real MULTIPROOF_GENESIS_OUTPUT_ROOT so registerAggregateVerifier initializes the
# AnchorStateRegistry with the true anchor (the env carries only the deploy-time placeholder).
echo ""
echo "--- Regenerating deploy-config.json ---"
mkdir -p "$WORKDIR/deploy-config"
MULTIPROOF_CONFIG_HASH="$MULTIPROOF_CONFIG_HASH_COMPUTED" \
MULTIPROOF_GENESIS_OUTPUT_ROOT="$MULTIPROOF_GENESIS_OUTPUT_ROOT" \
  envsubst <"$TEMPLATE_DIR/deploy-config.json.template" >"$WORKDIR/deploy-config/devnet.json"

echo ""
echo "--- Calling registerAggregateVerifier ---"
# Stage the shared deploy outfile into the foundry-writable /contracts/deployments dir so
# Artifacts.load reloads the existing addresses, then registerAggregateVerifier appends the new
# AggregateVerifier entry. forge writes back to LOCAL_DEPLOY_OUTFILE (a permitted path); we copy
# the updated file back to the shared volume afterwards.
mkdir -p "$WORKDIR/deployments"
cp "$DEPLOY_OUTFILE" "$LOCAL_DEPLOY_OUTFILE"
(
  cd "$WORKDIR"
  FOUNDRY_SCRIPT_EXECUTION_PROTECTION=false \
    DEPLOY_CONFIG_PATH="$WORKDIR/deploy-config/devnet.json" \
    DEPLOYMENT_OUTFILE="$LOCAL_DEPLOY_OUTFILE" \
    forge script scripts/deploy/SystemDeploy.s.sol:SystemDeploy \
    --sig "registerAggregateVerifier(bytes32)" "$MULTIPROOF_CONFIG_HASH_COMPUTED" \
    --sender "$DEPLOYER_ADDR" \
    --rpc-url "$L1_RPC_URL" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    --slow
)
# Persist the updated deploy outfile (now carrying the AggregateVerifier address) back to the
# shared volume for downstream consumers (smoke.sh, the l1-addresses.json refresh below).
cp "$LOCAL_DEPLOY_OUTFILE" "$DEPLOY_OUTFILE"

# Refresh l1-addresses.json so downstream consumers (and smoke.sh) see the AggregateVerifier
# address now that it has been registered.
if [ -f "$OUTPUT_DIR/l1-addresses.json" ]; then
  AGGREGATE_VERIFIER="$(jq -r '.AggregateVerifier // empty' "$DEPLOY_OUTFILE")"
  if [ -n "$AGGREGATE_VERIFIER" ]; then
    TMP_ADDRESSES="$(mktemp)"
    jq --arg av "$AGGREGATE_VERIFIER" '.AggregateVerifier = $av' \
      "$OUTPUT_DIR/l1-addresses.json" >"$TMP_ADDRESSES"
    mv "$TMP_ADDRESSES" "$OUTPUT_DIR/l1-addresses.json"
    echo "Updated l1-addresses.json AggregateVerifier = $AGGREGATE_VERIFIER"
  fi
fi

echo ""
echo "=== AggregateVerifier registration complete ==="
