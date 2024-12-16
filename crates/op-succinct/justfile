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

# Deploy mock verifier
deploy-mock-verifier env_file=".env":
    #!/usr/bin/env bash
    set -a
    source {{env_file}}
    set +a
    
    if [ -z "$L1_RPC" ]; then
        echo "L1_RPC not set in {{env_file}}"
        exit 1
    fi
    
    if [ -z "$PRIVATE_KEY" ]; then
        echo "PRIVATE_KEY not set in {{env_file}}"
        exit 1
    fi
    
    VERIFY_FLAGS=""
    if [ ! -z "$ETHERSCAN_API_KEY" ]; then
        VERIFY_FLAGS="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
    fi

    cd contracts
    
    forge script script/DeployMockVerifier.s.sol:DeployMockVerifier \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --broadcast \
    $VERIFY_FLAGS
# Deploy the OPSuccinct L2 Output Oracle
deploy-oracle env_file=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    
    # First fetch rollup config using the env file
    RUST_LOG=info cargo run --bin fetch-rollup-config --release -- --env-file {{env_file}}
    
    # Load environment variables
    source {{env_file}}

    # cd into contracts directory
    cd contracts

    # forge install
    forge install
    
    # Run the forge deployment script
    forge script script/OPSuccinctDeployer.s.sol:OPSuccinctDeployer \
        --rpc-url $L1_RPC \
        --private-key $PRIVATE_KEY \
        --broadcast \
        --verify \
        --verifier etherscan \
        --etherscan-api-key $ETHERSCAN_API_KEY


# Upgrade the OPSuccinct L2 Output Oracle
upgrade-oracle env_file=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    
    # First fetch rollup config using the env file
    RUST_LOG=info cargo run --bin fetch-rollup-config --release -- --env-file {{env_file}}
    
    # Load environment variables
    source {{env_file}}

    # cd into contracts directory
    cd contracts

    # forge install
    forge install
    
    # Run the forge upgrade script
    if [ "${EXECUTE_UPGRADE_CALL:-true}" = "false" ]; then
        L2OO_ADDRESS=$L2OO_ADDRESS forge script script/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY \
            --etherscan-api-key $ETHERSCAN_API_KEY
    else
        L2OO_ADDRESS=$L2OO_ADDRESS forge script script/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY \
            --verify \
            --verifier etherscan \
            --etherscan-api-key $ETHERSCAN_API_KEY \
            --broadcast
    fi

# Update the parameters of the OPSuccinct L2 Output Oracle
update-parameters env_file=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    
    # First fetch rollup config using the env file
    RUST_LOG=info cargo run --bin fetch-rollup-config --release -- --env-file {{env_file}}
    
    # Load environment variables
    source {{env_file}}

    # cd into contracts directory
    cd contracts

    # forge install
    forge install
    
    # Run the forge upgrade script
    if [ "${EXECUTE_UPGRADE_CALL:-true}" = "false" ]; then
        L2OO_ADDRESS=$L2OO_ADDRESS forge script script/OPSuccinctParameterUpdater.s.sol:OPSuccinctParameterUpdater \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY
    else
        L2OO_ADDRESS=$L2OO_ADDRESS forge script script/OPSuccinctParameterUpdater.s.sol:OPSuccinctParameterUpdater \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY \
            --broadcast
    fi