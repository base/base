#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/setup-l2-common.sh"

OUTPUT_DIR="${OUTPUT_DIR:-/output}"
SHARED_DIR="${SHARED_DIR:-/shared}"
CHAIN_ID="${CHAIN_ID:-1337}"
L1_CHAIN_ID="${L1_CHAIN_ID:-$CHAIN_ID}"
L2_CHAIN_ID="${L2_CHAIN_ID:-84538453}"
SLOT_DURATION="${SLOT_DURATION:-2}"
L1_DATA_DIR="${L1_DATA_DIR:-/data}"
TEMPLATE_DIR="${TEMPLATE_DIR:-/templates}"
PREALLOCATE_L2="${PREALLOCATE_L2:-false}"
L2_OUTPUT_DIR="${L2_OUTPUT_DIR:-/devnet/l2/configs}"

# Skip if L1 genesis already exists (for restarts)
if [ -f "$OUTPUT_DIR/el/genesis.json" ] && [ -f "$OUTPUT_DIR/cl/genesis.ssz" ]; then
  if [ "$PREALLOCATE_L2" = "true" ] && { [ ! -f "$L2_OUTPUT_DIR/genesis.json" ] || [ ! -f "$L2_OUTPUT_DIR/rollup.json" ] || [ ! -f "$L2_OUTPUT_DIR/l1-addresses.json" ]; }; then
    echo "ERROR: L1 genesis exists but preallocated L2 artifacts are missing."
    echo "Run devnet down before switching setup modes."
    exit 1
  fi
  echo "=== L1 Genesis already exists, skipping generation ==="
  exit 0
fi

case "$PREALLOCATE_L2" in
  true|false) ;;
  *)
    echo "ERROR: PREALLOCATE_L2 must be 'true' or 'false' (got '$PREALLOCATE_L2')"
    exit 1
    ;;
esac

if [ "$L1_CHAIN_ID" != "$CHAIN_ID" ]; then
  echo "ERROR: CHAIN_ID and L1_CHAIN_ID must match for L1 genesis generation"
  echo "CHAIN_ID=$CHAIN_ID"
  echo "L1_CHAIN_ID=$L1_CHAIN_ID"
  exit 1
fi

if [ "$PREALLOCATE_L2" = "true" ]; then
  setup_l2_common_load_defaults
  setup_l2_common_validate_activations
fi

# Anvil accounts balance: 1,000,000 ETH each (0xd3c21bcecceda1000000 = 1000000 * 10^18)
BALANCE="0xd3c21bcecceda1000000"

# Anvil's default test mnemonic for validators
VALIDATOR_MNEMONIC="test test test test test test test test test test test junk"

OP_DEPLOYER_WORKDIR=""
PREALLOC_L1_ALLOC_FILE=""

cleanup_preallocated_l2_workdir() {
  if [ -n "$OP_DEPLOYER_WORKDIR" ]; then
    rm -rf "$OP_DEPLOYER_WORKDIR"
  fi
  if [ -n "$PREALLOC_L1_ALLOC_FILE" ]; then
    rm -f "$PREALLOC_L1_ALLOC_FILE"
  fi
}

generate_preallocated_l2_artifacts() {
  echo ""
  echo "=== Running op-deployer (Genesis Mode) ==="

  mkdir -p "$L2_OUTPUT_DIR"

  OP_DEPLOYER_WORKDIR=$(mktemp -d)
  PREALLOC_L1_ALLOC_FILE=$(mktemp)
  echo "op-deployer working directory: $OP_DEPLOYER_WORKDIR"

  echo "Running op-deployer init..."
  op-deployer init \
    --l1-chain-id "$L1_CHAIN_ID" \
    --l2-chain-ids "$L2_CHAIN_ID" \
    --intent-type custom \
    --workdir "$OP_DEPLOYER_WORKDIR"

  INTENT_FILE="$OP_DEPLOYER_WORKDIR/intent.toml"
  echo "Configuring intent.toml for devnet..."

  L2_CHAIN_ID_HEX=$(printf "0x%064x" "$L2_CHAIN_ID")
  export L1_CHAIN_ID L2_CHAIN_ID_HEX DEPLOYER_ADDR SEQUENCER_ADDR BATCHER_ADDR PROPOSER_ADDR CHALLENGER_ADDR SEQ1_P2P_KEY SEQ2_P2P_KEY

  envsubst <"$TEMPLATE_DIR/l2-intent.toml.template" >"$INTENT_FILE"

  echo "Intent configured:"
  cat "$INTENT_FILE"

  echo ""
  echo "Running op-deployer apply (genesis mode)..."
  unset L1_RPC_URL
  op-deployer apply \
    --workdir "$OP_DEPLOYER_WORKDIR" \
    --deployment-target genesis \
    --private-key "$DEPLOYER_KEY"

  if [ ! -f "$OP_DEPLOYER_WORKDIR/state.json" ]; then
    echo "ERROR: op-deployer did not create state.json"
    ls -la "$OP_DEPLOYER_WORKDIR"
    exit 1
  fi

  OP_DEPLOYER_L1_TIMESTAMP_HEX=$(jq -re '.opChainDeployments[0].startBlock.timestamp' "$OP_DEPLOYER_WORKDIR/state.json")
  GENESIS_TIME=$((OP_DEPLOYER_L1_TIMESTAMP_HEX))
  GENESIS_TIME_HEX=$(printf "0x%x" "$GENESIS_TIME")

  echo "op-deployer state.json created successfully"
  echo "Preallocated L1 genesis timestamp: $GENESIS_TIME ($GENESIS_TIME_HEX)"

  echo "Extracting preallocated L1 alloc..."
  jq -r '.l1StateDump' "$OP_DEPLOYER_WORKDIR/state.json" | base64 -d | gzip -dc >"$PREALLOC_L1_ALLOC_FILE"
  echo "Preallocated L1 alloc written to $PREALLOC_L1_ALLOC_FILE"

  echo ""
  echo "=== Extracting L2 Configs ==="

  echo "Extracting L2 genesis..."
  op-deployer inspect genesis \
    --workdir "$OP_DEPLOYER_WORKDIR" \
    "$L2_CHAIN_ID" \
    >"$L2_OUTPUT_DIR/genesis.json"
  echo "L2 genesis written to $L2_OUTPUT_DIR/genesis.json"

  echo "Extracting rollup config..."
  op-deployer inspect rollup \
    --workdir "$OP_DEPLOYER_WORKDIR" \
    "$L2_CHAIN_ID" \
    >"$L2_OUTPUT_DIR/rollup.json"
  echo "Rollup config written to $L2_OUTPUT_DIR/rollup.json"

  setup_l2_common_patch_artifacts "$L2_OUTPUT_DIR"
  setup_l2_common_write_rollup_conductor_config "$L2_OUTPUT_DIR"

  echo "Extracting L1 addresses..."
  op-deployer inspect l1 \
    --workdir "$OP_DEPLOYER_WORKDIR" \
    "$L2_CHAIN_ID" \
    >"$L2_OUTPUT_DIR/l1-addresses.json"
  echo "L1 addresses written to $L2_OUTPUT_DIR/l1-addresses.json"

  setup_l2_common_write_p2p_keys "$L2_OUTPUT_DIR"
}

echo "=== Ethereum L1 Devnet Genesis Generator ==="
echo "Chain ID: $CHAIN_ID"
echo "Preallocate L2 into L1 genesis: $PREALLOCATE_L2"
if [ "$PREALLOCATE_L2" = "true" ]; then
  echo "L2 Chain ID: $L2_CHAIN_ID"
  echo "L2 output directory: $L2_OUTPUT_DIR"
  setup_l2_common_print_activation_config
fi
echo "Output directory: $OUTPUT_DIR"

mkdir -p "$OUTPUT_DIR/el" "$OUTPUT_DIR/cl" "$OUTPUT_DIR/l2"

if [ "$PREALLOCATE_L2" = "true" ]; then
  trap cleanup_preallocated_l2_workdir EXIT
  generate_preallocated_l2_artifacts
else
  # Generate timestamp
  GENESIS_TIME=$(date +%s)
  GENESIS_TIME_HEX=$(printf "0x%x" "$GENESIS_TIME")
fi
echo "Genesis time: $GENESIS_TIME ($GENESIS_TIME_HEX)"

# Save timestamp for other services
echo "$GENESIS_TIME" > "$SHARED_DIR/genesis_timestamp"

# =============================================================================
# Generate JWT Secret
# =============================================================================
echo ""
echo "=== Generating JWT Secret ==="

# Generate a random 32-byte hex secret for Engine API authentication
openssl rand -hex 32 > "$OUTPUT_DIR/jwt.hex"
echo "JWT secret written to $OUTPUT_DIR/jwt.hex"

# =============================================================================
# Generate EL (Execution Layer) Genesis with Prefunded Accounts
# =============================================================================
echo ""
echo "=== Generating Execution Layer Genesis ==="

# Export variables for envsubst
export CHAIN_ID GENESIS_TIME_HEX BALANCE

BASE_L1_GENESIS=$(mktemp)
envsubst < "$TEMPLATE_DIR/l1-el-genesis.json.template" > "$BASE_L1_GENESIS"

if [ "$PREALLOCATE_L2" = "true" ]; then
  echo "Merging preallocated L1 contract alloc into EL genesis..."
  jq --slurpfile prealloc "$PREALLOC_L1_ALLOC_FILE" '
    (.alloc | to_entries) as $base_entries
    | .alloc = (
        reduce $base_entries[] as $entry
          ({}; .[$entry.key | ascii_downcase] = ((.[$entry.key | ascii_downcase] // {}) + $entry.value))
      )
    | .alloc = (
        reduce ($prealloc[0] | to_entries[]) as $entry
          (.alloc; .[$entry.key | ascii_downcase] = ((.[$entry.key | ascii_downcase] // {}) + $entry.value))
      )
  ' "$BASE_L1_GENESIS" > "$OUTPUT_DIR/el/genesis.json"
else
  mv "$BASE_L1_GENESIS" "$OUTPUT_DIR/el/genesis.json"
fi

jq '.config' "$OUTPUT_DIR/el/genesis.json" > "$OUTPUT_DIR/el/chain-config.json"

echo "EL genesis written to $OUTPUT_DIR/el/genesis.json"
echo "L1 chain config written to $OUTPUT_DIR/el/chain-config.json"

# =============================================================================
# Generate CL (Consensus Layer) Genesis
# =============================================================================
echo ""
echo "=== Generating Consensus Layer Genesis ==="

# Export variables for envsubst
export GENESIS_TIME SLOT_DURATION

envsubst < "$TEMPLATE_DIR/l1-cl-config.yaml.template" > "$OUTPUT_DIR/cl/config.yaml"

echo "CL config written to $OUTPUT_DIR/cl/config.yaml"

# Create mnemonics file (Anvil's default mnemonic, 1 validator)
cat > "$OUTPUT_DIR/cl/mnemonics.yaml" << EOF
- mnemonic: "$VALIDATOR_MNEMONIC"
  count: 1
EOF

echo "Mnemonics written to $OUTPUT_DIR/cl/mnemonics.yaml"

# Generate CL genesis state
echo "Generating beacon chain genesis state..."
eth-genesis-state-generator beaconchain \
  --eth1-config "$OUTPUT_DIR/el/genesis.json" \
  --config "$OUTPUT_DIR/cl/config.yaml" \
  --mnemonics "$OUTPUT_DIR/cl/mnemonics.yaml" \
  --state-output "$OUTPUT_DIR/cl/genesis.ssz"

echo "CL genesis state written to $OUTPUT_DIR/cl/genesis.ssz"

# Generate validator keystores using eth2-val-tools
echo "Generating validator keystores..."

# Remove any existing validator output to avoid conflicts
rm -rf "$OUTPUT_DIR/cl/validator_keys"
rm -rf "$OUTPUT_DIR/cl/validator_data"

# Generate keystores for validator index 0 (we only have 1 validator)
# eth2-val-tools creates keys/ and secrets/ subdirectories
eth2-val-tools keystores \
  --insecure \
  --source-mnemonic="$VALIDATOR_MNEMONIC" \
  --source-min=0 \
  --source-max=1 \
  --out-loc="$OUTPUT_DIR/cl/validator_keys"

# Reorganize into Lighthouse validator data directory structure
# Lighthouse expects: datadir/validators/<pubkey>/voting-keystore.json
#                     datadir/secrets/<pubkey> (file containing password)
mkdir -p "$OUTPUT_DIR/cl/validator_data/validators"
mkdir -p "$OUTPUT_DIR/cl/validator_data/secrets"

# Move keys and secrets to the expected structure
for keydir in "$OUTPUT_DIR/cl/validator_keys/keys/"*; do
  if [ -d "$keydir" ]; then
    pubkey=$(basename "$keydir")
    mkdir -p "$OUTPUT_DIR/cl/validator_data/validators/$pubkey"
    cp "$keydir/voting-keystore.json" "$OUTPUT_DIR/cl/validator_data/validators/$pubkey/"

    # Copy the secret (password) file
    if [ -f "$OUTPUT_DIR/cl/validator_keys/secrets/$pubkey" ]; then
      cp "$OUTPUT_DIR/cl/validator_keys/secrets/$pubkey" "$OUTPUT_DIR/cl/validator_data/secrets/"
    fi
  fi
done

echo "Validator data written to $OUTPUT_DIR/cl/validator_data"

# Create required files for Lighthouse
echo "0" > "$OUTPUT_DIR/cl/deploy_block.txt"
echo "0" > "$OUTPUT_DIR/cl/deposit_contract_block.txt"

echo ""
echo "=== L1 Genesis Generation Complete ==="
echo ""
echo "Files generated:"
echo "  EL: $OUTPUT_DIR/el/genesis.json (with prefunded accounts)"
echo "  CL: $OUTPUT_DIR/cl/config.yaml"
echo "  CL: $OUTPUT_DIR/cl/genesis.ssz"
echo "  CL: $OUTPUT_DIR/cl/mnemonics.yaml"
echo "  JWT: $OUTPUT_DIR/jwt.hex"
echo ""
if [ "$PREALLOCATE_L2" = "true" ]; then
  echo "L2 genesis was generated with preallocated L1 state."
else
  echo "L2 genesis will be generated by setup-l2 after L1 is running."
fi
