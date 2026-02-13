#!/usr/bin/env bash

# Source devnet-env if it exists
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$SCRIPT_DIR/../../docker/devnet-env"
if [ -f "$ENV_FILE" ]; then
    set -a
    source "$ENV_FILE"
    set +a
fi

# Build accounts array from env vars
ACCOUNTS=(
    "$ANVIL_ACCOUNT_0_ADDR:$ANVIL_ACCOUNT_0_KEY"
    "$ANVIL_ACCOUNT_1_ADDR:$ANVIL_ACCOUNT_1_KEY"
    "$ANVIL_ACCOUNT_2_ADDR:$ANVIL_ACCOUNT_2_KEY"
    "$ANVIL_ACCOUNT_3_ADDR:$ANVIL_ACCOUNT_3_KEY"
    "$ANVIL_ACCOUNT_4_ADDR:$ANVIL_ACCOUNT_4_KEY"
    "$ANVIL_ACCOUNT_5_ADDR:$ANVIL_ACCOUNT_5_KEY"
    "$ANVIL_ACCOUNT_6_ADDR:$ANVIL_ACCOUNT_6_KEY"
    "$ANVIL_ACCOUNT_7_ADDR:$ANVIL_ACCOUNT_7_KEY"
    "$ANVIL_ACCOUNT_8_ADDR:$ANVIL_ACCOUNT_8_KEY"
    "$ANVIL_ACCOUNT_9_ADDR:$ANVIL_ACCOUNT_9_KEY"
)

get_role() {
    local addr=$1
    case "$addr" in
        "$DEPLOYER_ADDR") echo "DEPLOYER" ;;
        "$SEQUENCER_ADDR") echo "SEQUENCER" ;;
        "$BATCHER_ADDR") echo "BATCHER" ;;
        "$PROPOSER_ADDR") echo "PROPOSER" ;;
        "$CHALLENGER_ADDR") echo "CHALLENGER" ;;
        *) echo "" ;;
    esac
}
