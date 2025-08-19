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
  VERIFY=""
  ETHERSCAN_API_KEY="{{etherscan_api_key}}"
  if [ $ETHERSCAN_API_KEY != "" ]; then
    VERIFY="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
  fi

  L1_RPC="{{l1_rpc}}"
  ADMIN_PK="{{admin_pk}}"

  cd contracts && forge script script/validity/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader  --rpc-url $L1_RPC --private-key $ADMIN_PK $VERIFY --broadcast --slow

# Deploy OPSuccinct FDG contracts
deploy-fdg-contracts env_file=".env" *features='':
    #!/usr/bin/env bash
    set -euo pipefail
    
    # First fetch FDG config using the env file
    echo "Fetching Fault Dispute Game configuration..."
    if [ -z "{{features}}" ]; then
        RUST_LOG=info cargo run --bin fetch-fault-dispute-game-config --release -- --env-file {{env_file}}
    else
        echo "Fetching fault dispute game config with features: {{features}}"
        RUST_LOG=info cargo run --bin fetch-fault-dispute-game-config --release --features {{features}} -- --env-file {{env_file}}
    fi
    
    # Load environment variables from project root
    source {{env_file}}
    
    # Load environment variables from contracts directory if it exists
    if [ -f "contracts/.env" ]; then
        source contracts/.env
    fi
    
    # Check if required environment variables are set
    if [ -z "${RPC_URL:-}" ] && [ -z "${L1_RPC:-}" ]; then
        echo "Error: Neither RPC_URL nor L1_RPC environment variable is set"
        exit 1
    fi
    
    if [ -z "${PRIVATE_KEY:-}" ]; then
        echo "Error: PRIVATE_KEY environment variable is not set"
        exit 1
    fi
    
    # Use RPC_URL if set, otherwise fall back to L1_RPC
    RPC_URL_TO_USE="${RPC_URL:-$L1_RPC}"
    echo "Using RPC URL: $RPC_URL_TO_USE"

    echo "Deploying FDG contracts..."
    
    # Change to contracts directory
    cd contracts
    
    # Install dependencies
    echo "Installing forge dependencies..."
    forge install
    
    # Build contracts
    echo "Building contracts..."
    forge build
    
    # Setup verification flags
    VERIFY=""
    if [ -n "${ETHERSCAN_API_KEY:-}" ]; then
        VERIFY="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY --retries 10 --delay 5"
        echo "Verification enabled with Etherscan"
    fi
    
    # Run deployment script
    echo "Running deployment script..."
    forge script script/fp/DeployOPSuccinctFDG.s.sol \
        --broadcast \
        --rpc-url "$RPC_URL_TO_USE" \
        --private-key "$PRIVATE_KEY" \
        $VERIFY
    
    echo "FDG contract deployment complete!"

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
deploy-oracle env_file=".env" *features='':
    #!/usr/bin/env bash
    set -euo pipefail
    
    # First fetch rollup config using the env file
    if [ -z "{{features}}" ]; then
        RUST_LOG=info cargo run --bin fetch-l2oo-config --release -- --env-file {{env_file}}
    else
        echo "Fetching rollup config with features: {{features}}"
        RUST_LOG=info cargo run --bin fetch-l2oo-config --release --features {{features}} -- --env-file {{env_file}}
    fi
    
    # Load environment variables
    source {{env_file}}

    # cd into contracts directory
    cd contracts

    VERIFY=""
    if [ "$ETHERSCAN_API_KEY" != "" ]; then
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
upgrade-oracle env_file=".env" *features='':
    #!/usr/bin/env bash
    set -euo pipefail
    
    # First fetch rollup config using the env file
    if [ -z "{{features}}" ]; then
        RUST_LOG=info cargo run --bin fetch-l2oo-config --release -- --env-file {{env_file}}
    else
        echo "Fetching rollup config with features: {{features}}"
        RUST_LOG=info cargo run --bin fetch-l2oo-config --release --features {{features}} -- --env-file {{env_file}}
    fi
    
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

    
    
    VERIFY_FLAGS=""
    if [ -n "${ETHERSCAN_API_KEY:-}" ]; then
        VERIFY_FLAGS="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
    fi

    if [ "${EXECUTE_UPGRADE_CALL:-true}" = "false" ]; then
        env $ENV_VARS forge script script/validity/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY
    else
        env $ENV_VARS forge script script/validity/OPSuccinctUpgrader.s.sol:OPSuccinctUpgrader \
            --rpc-url $L1_RPC \
            --private-key $PRIVATE_KEY \
            $VERIFY_FLAGS \
            --broadcast
    fi

deploy-dispute-game-factory env_file=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    
    # Load environment variables
    source {{env_file}}

    # Check if required environment variables are set.
    if [ -z "${L2OO_ADDRESS:-}" ]; then
        echo "Error: L2OO_ADDRESS environment variable is not set"
        exit 1
    fi
    if [ -z "${PROPOSER_ADDRESSES:-}" ]; then
        echo "Error: PROPOSER_ADDRESSES environment variable is not set"
        exit 1
    fi

    # cd into contracts directory
    cd contracts

    # forge install
    forge install

    VERIFY=""
    if [ -n "$ETHERSCAN_API_KEY" ]; then
      VERIFY="--verify --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY"
    fi
    
    # Run the forge deployment script
    env L2OO_ADDRESS=$L2OO_ADDRESS \
        PROPOSER_ADDRESSES=$PROPOSER_ADDRESSES \
        forge script script/validity/OPSuccinctDGFDeployer.s.sol:OPSuccinctDFGDeployer \
        --rpc-url $L1_RPC \
        --private-key $PRIVATE_KEY \
        --broadcast \
        $VERIFY

# Add a new OpSuccinctConfig to the L2 Output Oracle
add-config config_name env_file=".env" *features='':
    #!/usr/bin/env bash
    set -euo pipefail
    
    # First fetch rollup config using the env file
    if [ -z "{{features}}" ]; then
        RUST_LOG=info cargo run --bin fetch-l2oo-config --release -- --env-file {{env_file}}
    else
        echo "Fetching rollup config with features: {{features}}"
        RUST_LOG=info cargo run --bin fetch-l2oo-config --release --features {{features}} -- --env-file {{env_file}}
    fi
    
    # Load environment variables
    source {{env_file}}

    # cd into contracts directory
    cd contracts

    # forge install
    forge install
    
    # Run the forge script to add config
    env L2OO_ADDRESS="$L2OO_ADDRESS" \
        ${EXECUTE_UPGRADE_CALL:+EXECUTE_UPGRADE_CALL="$EXECUTE_UPGRADE_CALL"} \
        ${ADMIN_PK:+ADMIN_PK="$ADMIN_PK"} \
        ${DEPLOY_PK:+DEPLOY_PK="$DEPLOY_PK"} \
        forge script script/validity/OPSuccinctParameterUpdater.s.sol:OPSuccinctParameterUpdater \
        --sig "addConfig(string)" "{{config_name}}" \
        --rpc-url $L1_RPC \
        --private-key $PRIVATE_KEY \
        --broadcast

# Remove an OpSuccinctConfig from the L2 Output Oracle  
remove-config config_name env_file=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    
    # Load environment variables
    source {{env_file}}

    # cd into contracts directory
    cd contracts

    # forge install
    forge install
    
    # Run the forge script to remove config
    env L2OO_ADDRESS="$L2OO_ADDRESS" \
        ${EXECUTE_UPGRADE_CALL:+EXECUTE_UPGRADE_CALL="$EXECUTE_UPGRADE_CALL"} \
        ${ADMIN_PK:+ADMIN_PK="$ADMIN_PK"} \
        ${DEPLOY_PK:+DEPLOY_PK="$DEPLOY_PK"} \
        forge script script/validity/OPSuccinctParameterUpdater.s.sol:OPSuccinctParameterUpdater \
        --sig "removeConfig(string)" "{{config_name}}" \
        --rpc-url $L1_RPC \
        --private-key $PRIVATE_KEY \
        --broadcast
