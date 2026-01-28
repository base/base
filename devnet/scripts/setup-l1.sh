#!/bin/bash
set -e

OUTPUT_DIR="${OUTPUT_DIR:-/output}"
SHARED_DIR="${SHARED_DIR:-/shared}"
CHAIN_ID="${CHAIN_ID:-1337}"
SLOT_DURATION="${SLOT_DURATION:-2}"
L1_DATA_DIR="${L1_DATA_DIR:-/data}"
TEMPLATE_DIR="${TEMPLATE_DIR:-/templates}"

# Skip if genesis already exists (for restarts)
if [ -f "$OUTPUT_DIR/el/genesis.json" ] && [ -f "$OUTPUT_DIR/cl/genesis.ssz" ]; then
  echo "=== L1 Genesis already exists, skipping generation ==="
  exit 0
fi

# Clean up any partial/stale data for fresh start
echo "=== Cleaning up existing L1 data ==="
rm -rf "${OUTPUT_DIR:?}"/el/*
rm -rf "${OUTPUT_DIR:?}"/cl/*
rm -rf "${OUTPUT_DIR:?}"/jwt.hex
rm -rf "${OUTPUT_DIR:?}"/genesis_timestamp
rm -rf "${L1_DATA_DIR:?}"/l1-reth/*
rm -rf "${L1_DATA_DIR:?}"/l1-lighthouse/*

# Anvil accounts balance: 1,000,000 ETH each (0xd3c21bcecceda1000000 = 1000000 * 10^18)
BALANCE="0xd3c21bcecceda1000000"

# Anvil's default test mnemonic for validators
VALIDATOR_MNEMONIC="test test test test test test test test test test test junk"

echo "=== Ethereum L1 Devnet Genesis Generator ==="
echo "Chain ID: $CHAIN_ID"
echo "Output directory: $OUTPUT_DIR"

# Generate timestamp
GENESIS_TIME=$(date +%s)
GENESIS_TIME_HEX=$(printf "0x%x" $GENESIS_TIME)
echo "Genesis time: $GENESIS_TIME ($GENESIS_TIME_HEX)"

# Save timestamp for other services
echo "$GENESIS_TIME" > "$SHARED_DIR/genesis_timestamp"

mkdir -p "$OUTPUT_DIR/el" "$OUTPUT_DIR/cl" "$OUTPUT_DIR/l2"

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

envsubst < "$TEMPLATE_DIR/l1-el-genesis.json.template" > "$OUTPUT_DIR/el/genesis.json"

echo "EL genesis written to $OUTPUT_DIR/el/genesis.json"

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
echo "L2 genesis will be generated by setup-l2 after L1 is running."
