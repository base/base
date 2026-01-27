#!/bin/bash
set -e

OUTPUT_DIR="${OUTPUT_DIR:-/output}"
SHARED_DIR="${SHARED_DIR:-/shared}"
CHAIN_ID="${CHAIN_ID:-1337}"
SLOT_DURATION="${SLOT_DURATION:-2}"
L1_DATA_DIR="${L1_DATA_DIR:-/data}"

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

# Anvil accounts with 1,000,000 ETH each (0xd3c21bcecceda1000000 = 1000000 * 10^18)
ANVIL_ACCOUNTS=(
  "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
  "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
  "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
  "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
  "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
  "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
  "0x976EA74026E726554dB657fA54763abd0C3a0aa9"
  "0x14dC79964da2C08b23698B3D3cc7Ca32193d9955"
  "0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f"
  "0xa0Ee7A142d267C1f36714E4a8F75612F20a79720"
)

BALANCE="0xd3c21bcecceda1000000"

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

# Create L1 genesis with prefunded Anvil accounts
# OP contracts will be deployed live to the running L1 by setup-l2
cat > "$OUTPUT_DIR/el/genesis.json" << EOF
{
  "config": {
    "chainId": $CHAIN_ID,
    "homesteadBlock": 0,
    "eip150Block": 0,
    "eip155Block": 0,
    "eip158Block": 0,
    "byzantiumBlock": 0,
    "constantinopleBlock": 0,
    "petersburgBlock": 0,
    "istanbulBlock": 0,
    "berlinBlock": 0,
    "londonBlock": 0,
    "arrowGlacierBlock": 0,
    "grayGlacierBlock": 0,
    "terminalTotalDifficulty": 0,
    "shanghaiTime": 0,
    "cancunTime": 0,
    "pragueTime": 0,
    "osakaTime": 0,
    "blobSchedule": {
      "cancun": { "target": 3, "max": 6, "baseFeeUpdateFraction": 3338477 },
      "prague": { "target": 6, "max": 9, "baseFeeUpdateFraction": 5007716 },
      "osaka": { "target": 9, "max": 12, "baseFeeUpdateFraction": 5007716 },
      "bpo1": { "target": 10, "max": 15, "baseFeeUpdateFraction": 5007716 },
      "bpo2": { "target": 14, "max": 21, "baseFeeUpdateFraction": 5007716 }
    },
    "bpo1Time": 0,
    "bpo2Time": 0
  },
  "nonce": "0x0",
  "timestamp": "$GENESIS_TIME_HEX",
  "extraData": "0x",
  "gasLimit": "0x1c9c380",
  "difficulty": "0x0",
  "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "coinbase": "0x0000000000000000000000000000000000000000",
  "alloc": {
    "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266": {"balance": "$BALANCE"},
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8": {"balance": "$BALANCE"},
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC": {"balance": "$BALANCE"},
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906": {"balance": "$BALANCE"},
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65": {"balance": "$BALANCE"},
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc": {"balance": "$BALANCE"},
    "0x976EA74026E726554dB657fA54763abd0C3a0aa9": {"balance": "$BALANCE"},
    "0x14dC79964da2C08b23698B3D3cc7Ca32193d9955": {"balance": "$BALANCE"},
    "0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f": {"balance": "$BALANCE"},
    "0xa0Ee7A142d267C1f36714E4a8F75612F20a79720": {"balance": "$BALANCE"},
    "0x4e59b44847b379578588920cA78FbF26c0B4956C": {
      "balance": "0x0",
      "code": "0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3"
    }
  },
  "baseFeePerGas": "0x3b9aca00",
  "blobGasUsed": "0x0",
  "excessBlobGas": "0x0"
}
EOF

echo "EL genesis written to $OUTPUT_DIR/el/genesis.json"

# =============================================================================
# Generate CL (Consensus Layer) Genesis
# =============================================================================
echo ""
echo "=== Generating Consensus Layer Genesis ==="

# Create CL config (complete minimal preset with all required fields)
cat > "$OUTPUT_DIR/cl/config.yaml" << EOF
# Extends the minimal preset
PRESET_BASE: minimal
CONFIG_NAME: devnet

# Terminal PoW block
TERMINAL_TOTAL_DIFFICULTY: 0
TERMINAL_BLOCK_HASH: 0x0000000000000000000000000000000000000000000000000000000000000000
TERMINAL_BLOCK_HASH_ACTIVATION_EPOCH: 18446744073709551615

# Genesis
MIN_GENESIS_ACTIVE_VALIDATOR_COUNT: 1
MIN_GENESIS_TIME: $GENESIS_TIME
GENESIS_FORK_VERSION: 0x10000000
GENESIS_DELAY: 0

# Forking
ALTAIR_FORK_VERSION: 0x20000000
ALTAIR_FORK_EPOCH: 0
BELLATRIX_FORK_VERSION: 0x30000000
BELLATRIX_FORK_EPOCH: 0
CAPELLA_FORK_VERSION: 0x40000000
CAPELLA_FORK_EPOCH: 0
DENEB_FORK_VERSION: 0x50000000
DENEB_FORK_EPOCH: 0
ELECTRA_FORK_VERSION: 0x60000000
ELECTRA_FORK_EPOCH: 0
FULU_FORK_VERSION: 0x70000000
FULU_FORK_EPOCH: 0

# Time parameters
SECONDS_PER_SLOT: $SLOT_DURATION
SECONDS_PER_ETH1_BLOCK: 14
MIN_VALIDATOR_WITHDRAWABILITY_DELAY: 256
SHARD_COMMITTEE_PERIOD: 64
ETH1_FOLLOW_DISTANCE: 16

# Validator cycle
INACTIVITY_SCORE_BIAS: 4
INACTIVITY_SCORE_RECOVERY_RATE: 16
EJECTION_BALANCE: 16000000000
MIN_PER_EPOCH_CHURN_LIMIT: 2
CHURN_LIMIT_QUOTIENT: 32
MAX_PER_EPOCH_ACTIVATION_CHURN_LIMIT: 4

# Fork choice
PROPOSER_SCORE_BOOST: 40
REORG_HEAD_WEIGHT_THRESHOLD: 20
REORG_PARENT_WEIGHT_THRESHOLD: 160
REORG_MAX_EPOCHS_SINCE_FINALIZATION: 2

# Deposit contract
DEPOSIT_CHAIN_ID: $CHAIN_ID
DEPOSIT_NETWORK_ID: $CHAIN_ID
DEPOSIT_CONTRACT_ADDRESS: 0x0000000000000000000000000000000000000000

# Networking
MAX_PAYLOAD_SIZE: 10485760
MAX_REQUEST_BLOCKS: 1024
EPOCHS_PER_SUBNET_SUBSCRIPTION: 256
MIN_EPOCHS_FOR_BLOCK_REQUESTS: 272
ATTESTATION_PROPAGATION_SLOT_RANGE: 32
MAXIMUM_GOSSIP_CLOCK_DISPARITY: 500
MESSAGE_DOMAIN_INVALID_SNAPPY: 0x00000000
MESSAGE_DOMAIN_VALID_SNAPPY: 0x01000000
SUBNETS_PER_NODE: 2
ATTESTATION_SUBNET_COUNT: 64
ATTESTATION_SUBNET_EXTRA_BITS: 0
ATTESTATION_SUBNET_PREFIX_BITS: 6

# Deneb
MAX_REQUEST_BLOCKS_DENEB: 128
MIN_EPOCHS_FOR_BLOB_SIDECARS_REQUESTS: 4096
BLOB_SIDECAR_SUBNET_COUNT: 6
MAX_BLOBS_PER_BLOCK: 6
MAX_REQUEST_BLOB_SIDECARS: 768

# Electra
MAX_BLOBS_PER_BLOCK_ELECTRA: 9
BLOB_SIDECAR_SUBNET_COUNT_ELECTRA: 9
MAX_REQUEST_BLOB_SIDECARS_ELECTRA: 1152
MAX_EFFECTIVE_BALANCE_ELECTRA: 2048000000000
MIN_ACTIVATION_BALANCE: 32000000000
MIN_SLASHING_PENALTY_QUOTIENT_ELECTRA: 4096
WHISTLEBLOWER_REWARD_QUOTIENT_ELECTRA: 4096
MAX_ATTESTER_SLASHINGS_ELECTRA: 1
MAX_ATTESTATIONS_ELECTRA: 8
MAX_PENDING_PARTIALS_PER_WITHDRAWALS_SWEEP: 8
MAX_PENDING_DEPOSITS_PER_EPOCH: 16
PENDING_DEPOSITS_LIMIT: 134217728
PENDING_PARTIAL_WITHDRAWALS_LIMIT: 134217728
PENDING_CONSOLIDATIONS_LIMIT: 262144
MIN_PER_EPOCH_CHURN_LIMIT_ELECTRA: 128000000000
MAX_PER_EPOCH_ACTIVATION_EXIT_CHURN_LIMIT: 256000000000

# Fulu (with BPO2 parameters: target 14, max 21)
MAX_BLOBS_PER_BLOCK_FULU: 21
BLOB_SIDECAR_SUBNET_COUNT_FULU: 21
MAX_REQUEST_BLOB_SIDECARS_FULU: 2688

EOF

echo "CL config written to $OUTPUT_DIR/cl/config.yaml"

# Create mnemonics file (Anvil's default mnemonic)
cat > "$OUTPUT_DIR/cl/mnemonics.yaml" << EOF
- mnemonic: "test test test test test test test test test test test junk"
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
  --source-mnemonic="test test test test test test test test test test test junk" \
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
echo "Prefunded accounts (1,000,000 ETH each):"
for addr in "${ANVIL_ACCOUNTS[@]}"; do
  echo "  $addr"
done
echo ""
echo "L2 genesis will be generated by setup-l2 after L1 is running."
