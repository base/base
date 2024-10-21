set dotenv-load

default:
  @just --list

# Runs the op-succinct program for a single block.
run-single l2_block_num use-cache="false" prove="false":
  #!/usr/bin/env bash
  CACHE_FLAG=""
  if [ "{{use-cache}}" = "true" ]; then
    CACHE_FLAG="--use-cache"
  fi
  PROVE_FLAG=""
  if [ "{{prove}}" = "true" ]; then
    PROVE_FLAG="--prove"
  fi
  cargo run --bin single --release -- --l2-block {{l2_block_num}} $CACHE_FLAG $PROVE_FLAG

# Runs the op-succinct program for multiple blocks.
run-multi start end use-cache="false" prove="false":
  #!/usr/bin/env bash
  CACHE_FLAG=""
  if [ "{{use-cache}}" = "true" ]; then
    CACHE_FLAG="--use-cache"
  fi
  PROVE_FLAG=""
  if [ "{{prove}}" = "true" ]; then
    PROVE_FLAG="--prove"
  fi

  cargo run --bin multi --release -- --start {{start}} --end {{end}} $CACHE_FLAG $PROVE_FLAG

# Runs the cost estimator for a given block range.
cost-estimator start end:
  #!/usr/bin/env bash
  cargo run --bin cost-estimator --release -- --start {{start}} --end {{end}}

# Runs the client program in native execution mode. Modified version of Kona Native Client execution:
# https://github.com/ethereum-optimism/kona/blob/ae71b9df103c941c06b0dc5400223c4f13fe5717/bin/client/justfile#L65-L108
run-client-native l2_block_num l1_rpc='${L1_RPC}' l1_beacon_rpc='${L1_BEACON_RPC}' l2_rpc='${L2_RPC}' verbosity="-vvvv":
  #!/usr/bin/env bash
  L1_NODE_ADDRESS="{{l1_rpc}}"
  L1_BEACON_ADDRESS="{{l1_beacon_rpc}}"
  L2_NODE_ADDRESS="{{l2_rpc}}"
  echo "L1 Node Address: $L1_NODE_ADDRESS"
  echo "L1 Beacon Address: $L1_BEACON_ADDRESS"
  echo "L2 Node Address: $L2_NODE_ADDRESS"
  HOST_BIN_PATH="./kona-host"
  CLIENT_BIN_PATH="$(pwd)/target/release-client-lto/fault-proof"
  L2_BLOCK_NUMBER="{{l2_block_num}}"
  L2_BLOCK_SAFE_HEAD=$((L2_BLOCK_NUMBER - 1))
  L2_OUTPUT_STATE_ROOT=$(cast block --rpc-url $L2_NODE_ADDRESS --field stateRoot $L2_BLOCK_SAFE_HEAD)
  L2_HEAD=$(cast block --rpc-url $L2_NODE_ADDRESS --field hash $L2_BLOCK_SAFE_HEAD)
  L2_OUTPUT_STORAGE_HASH=$(cast proof --rpc-url $L2_NODE_ADDRESS --block $L2_BLOCK_SAFE_HEAD 0x4200000000000000000000000000000000000016 | jq -r '.storageHash')
  L2_OUTPUT_ENCODED=$(cast abi-encode "x(uint256,bytes32,bytes32,bytes32)" 0 $L2_OUTPUT_STATE_ROOT $L2_OUTPUT_STORAGE_HASH $L2_HEAD)
  L2_OUTPUT_ROOT=$(cast keccak $L2_OUTPUT_ENCODED)
  echo "L2 Safe Head: $L2_BLOCK_SAFE_HEAD"
  echo "Safe Head Output Root: $L2_OUTPUT_ROOT"
  L2_CLAIM_STATE_ROOT=$(cast block --rpc-url $L2_NODE_ADDRESS --field stateRoot $L2_BLOCK_NUMBER)
  L2_CLAIM_HASH=$(cast block --rpc-url $L2_NODE_ADDRESS --field hash $L2_BLOCK_NUMBER)
  L2_CLAIM_STORAGE_HASH=$(cast proof --rpc-url $L2_NODE_ADDRESS --block $L2_BLOCK_NUMBER 0x4200000000000000000000000000000000000016 | jq -r '.storageHash')
  L2_CLAIM_ENCODED=$(cast abi-encode "x(uint256,bytes32,bytes32,bytes32)" 0 $L2_CLAIM_STATE_ROOT $L2_CLAIM_STORAGE_HASH $L2_CLAIM_HASH)
  L2_CLAIM=$(cast keccak $L2_CLAIM_ENCODED)
  echo "L2 Block Number: $L2_BLOCK_NUMBER"
  echo "L2 Claim Root: $L2_CLAIM"
  L2_BLOCK_TIMESTAMP=$(cast block --rpc-url $L2_NODE_ADDRESS $L2_BLOCK_NUMBER -j | jq -r .timestamp)
  L1_HEAD=$(cast block --rpc-url $L1_NODE_ADDRESS $(cast find-block --rpc-url $L1_NODE_ADDRESS $(($(cast 2d $L2_BLOCK_TIMESTAMP) + 300))) -j | jq -r .hash)
  echo "L1 Head: $L1_HEAD"
  L2_CHAIN_ID=10
  DATA_DIRECTORY="./data/$L2_BLOCK_NUMBER"
  echo "Saving Data to $DATA_DIRECTORY"
  echo "Building client program..."
  cargo build --bin fault-proof --profile release-client-lto
  echo "Running host program with native client program..."
  cargo run --bin op-succinct-witnessgen --release -- \
    --l1-head $L1_HEAD \
    --l2-head $L2_HEAD \
    --l2-claim $L2_CLAIM \
    --l2-output-root $L2_OUTPUT_ROOT \
    --l2-block-number $L2_BLOCK_NUMBER \
    --l2-chain-id $L2_CHAIN_ID \
    --l1-node-address $L1_NODE_ADDRESS \
    --l1-beacon-address $L1_BEACON_ADDRESS \
    --l2-node-address $L2_NODE_ADDRESS \
    --exec $CLIENT_BIN_PATH \
    --data-dir $DATA_DIRECTORY \
    {{verbosity}}

  # Output the data required for the ZKVM execution.
  echo "$L1_HEAD $L2_OUTPUT_ROOT $L2_CLAIM $L2_BLOCK_NUMBER $L2_CHAIN_ID"

upgrade-l2oo l1_rpc admin_pk etherscan_api_key="":
  #!/usr/bin/env bash

  CHAIN_ID=$(jq -r '.chainId' contracts/opsuccinctl2ooconfig.json)
  if [ "$CHAIN_ID" = "0" ] || [ -z "$CHAIN_ID" ]; then
    echo "Are you sure you've filled out your opsuccinctl2ooconfig.json? Your chain ID is currently set to 0."
    exit 1
  fi

  VERIFY=""
  ETHERSCAN_API_KEY="{{etherscan_api_key}}"
  if [ $ETHERSCAN_API_KEY != "" ]; then
    VERIFY="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
  fi

  L1_RPC="{{l1_rpc}}"
  ADMIN_PK="{{admin_pk}}"

  cd contracts && forge script script/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader  --rpc-url $L1_RPC --private-key $ADMIN_PK $VERIFY --broadcast --slow
