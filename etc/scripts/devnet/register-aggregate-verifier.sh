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
L1_CHAIN_ID="${L1_CHAIN_ID:-1337}"
OUTPUT_DIR="${OUTPUT_DIR:-/devnet/l2/configs}"
TEMPLATE_DIR="${TEMPLATE_DIR:-/templates}"
WORKDIR=/contracts
HASH_FILE="${HASH_FILE:-$OUTPUT_DIR/multiproof-config-hash}"
DEPLOY_OUTFILE="$OUTPUT_DIR/${L1_CHAIN_ID}-deploy.json"

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

# Regenerate the deploy config from the template exactly as setup-l2.sh step 1 did, so the
# verifier's non-hash inputs (teeImageHash, intervals, game type, …) match the original deploy.
# Override MULTIPROOF_CONFIG_HASH with the computed hash (the env carries only the deploy-time
# placeholder) so the regenerated devnet.json is self-consistent with the value we register —
# the forge arg below is authoritative, but this avoids a stale hash on disk being read silently.
echo ""
echo "--- Regenerating deploy-config.json ---"
mkdir -p "$WORKDIR/deploy-config"
MULTIPROOF_CONFIG_HASH="$MULTIPROOF_CONFIG_HASH_COMPUTED" \
  envsubst <"$TEMPLATE_DIR/deploy-config.json.template" >"$WORKDIR/deploy-config/devnet.json"

echo ""
echo "--- Calling registerAggregateVerifier ---"
(
  cd "$WORKDIR"
  # DEPLOYMENT_OUTFILE points Artifacts at the shared copy: registerAggregateVerifier reloads
  # the existing addresses from it and appends the new AggregateVerifier entry back to it.
  FOUNDRY_SCRIPT_EXECUTION_PROTECTION=false \
    DEPLOY_CONFIG_PATH="$WORKDIR/deploy-config/devnet.json" \
    DEPLOYMENT_OUTFILE="$DEPLOY_OUTFILE" \
    forge script scripts/deploy/SystemDeploy.s.sol:SystemDeploy \
    --sig "registerAggregateVerifier(bytes32)" "$MULTIPROOF_CONFIG_HASH_COMPUTED" \
    --sender "$DEPLOYER_ADDR" \
    --rpc-url "$L1_RPC_URL" \
    --private-key "$DEPLOYER_KEY" \
    --broadcast \
    --slow
)

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
