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
# If no range is provided, runs for the last 5 finalized blocks.
cost-estimator *args='':
  #!/usr/bin/env bash
  if [ -z "{{args}}" ]; then
    cargo run --bin cost-estimator --release
  else
    cargo run --bin cost-estimator --release -- {{args}}
  fi

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

  cd contracts && forge script script/validity/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader  --rpc-url $L1_RPC --private-key $ADMIN_PK $VERIFY --broadcast --slow

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

    cd contracts

    VERIFY=""
    if [ $ETHERSCAN_API_KEY != "" ]; then
      VERIFY="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
    fi
    
    forge script script/validity/DeployMockVerifier.s.sol:DeployMockVerifier \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --broadcast \
    $VERIFY

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

    VERIFY=""
    if [ $ETHERSCAN_API_KEY != "" ]; then
      VERIFY="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
    fi
    
    ENV_VARS=""
    if [ -n "${ADMIN_PK:-}" ]; then ENV_VARS="$ENV_VARS ADMIN_PK=$ADMIN_PK"; fi
    if [ -n "${DEPLOY_PK:-}" ]; then ENV_VARS="$ENV_VARS DEPLOY_PK=$DEPLOY_PK"; fi
    
    # Run the forge deployment script
    $ENV_VARS forge script script/validity/OPSuccinctDeployer.s.sol:OPSuccinctDeployer \
        --rpc-url $L1_RPC \
        --private-key $PRIVATE_KEY \
        --broadcast \
        $VERIFY

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
    
    ENV_VARS="L2OO_ADDRESS=$L2OO_ADDRESS"
    if [ -n "${EXECUTE_UPGRADE_CALL:-}" ]; then ENV_VARS="$ENV_VARS EXECUTE_UPGRADE_CALL=$EXECUTE_UPGRADE_CALL"; fi
    if [ -n "${ADMIN_PK:-}" ]; then ENV_VARS="$ENV_VARS ADMIN_PK=$ADMIN_PK"; fi
    if [ -n "${DEPLOY_PK:-}" ]; then ENV_VARS="$ENV_VARS DEPLOY_PK=$DEPLOY_PK"; fi
    
    if [ "${EXECUTE_UPGRADE_CALL:-true}" = "false" ]; then
        env $ENV_VARS forge script script/validity/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY \
            --etherscan-api-key $ETHERSCAN_API_KEY
    else
        env $ENV_VARS forge script script/validity/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
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
        env L2OO_ADDRESS="$L2OO_ADDRESS" \
            ${EXECUTE_UPGRADE_CALL:+EXECUTE_UPGRADE_CALL="$EXECUTE_UPGRADE_CALL"} \
            ${ADMIN_PK:+ADMIN_PK="$ADMIN_PK"} \
            ${DEPLOY_PK:+DEPLOY_PK="$DEPLOY_PK"} \
            forge script script/validity/OPSuccinctParameterUpdater.s.sol:OPSuccinctParameterUpdater \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY \
            --broadcast
    else
        env L2OO_ADDRESS="$L2OO_ADDRESS" \
            ${EXECUTE_UPGRADE_CALL:+EXECUTE_UPGRADE_CALL="$EXECUTE_UPGRADE_CALL"} \
            ${ADMIN_PK:+ADMIN_PK="$ADMIN_PK"} \
            ${DEPLOY_PK:+DEPLOY_PK="$DEPLOY_PK"} \
            forge script script/validity/OPSuccinctParameterUpdater.s.sol:OPSuccinctParameterUpdater \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY \
            --broadcast
    fi

deploy-dispute-game-factory env_file=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    
    # Load environment variables
    source {{env_file}}

    # cd into contracts directory
    cd contracts

    # forge install
    forge install

    VERIFY=""
    if [ $ETHERSCAN_API_KEY != "" ]; then
      VERIFY="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
    fi
    
    # Run the forge deployment script
    L2OO_ADDRESS=$L2OO_ADDRESS forge script script/validity/OPSuccinctDGFDeployer.s.sol:OPSuccinctDFGDeployer \
        --rpc-url $L1_RPC \
        --private-key $PRIVATE_KEY \
        --broadcast \
        $VERIFY
