#!/usr/bin/env bash
set -e

# Source .env.devnet if it exists
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$SCRIPT_DIR/../../.env.devnet"
if [ -f "$ENV_FILE" ]; then
    set -a
    source "$ENV_FILE"
    set +a
fi

L1_RPC="${1:-$L1_RPC_URL}"
L2_RPC="${2:-$L2_CLIENT_RPC_URL}"

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

print_table() {
    local rpc=$1
    local label=$2
    local show_roles=$3

    printf "\n%s (RPC: %s)\n" "$label" "$rpc"
    if [ "$show_roles" = "true" ]; then
        printf "%-12s | %-44s | %-68s | %18s | %8s\n" "Role" "Address" "Private Key" "Balance (ETH)" "Nonce"
        printf "%s\n" "-------------+----------------------------------------------+----------------------------------------------------------------------+--------------------+----------"
    else
        printf "%-44s | %-68s | %18s | %8s\n" "Address" "Private Key" "Balance (ETH)" "Nonce"
        printf "%s\n" "---------------------------------------------+----------------------------------------------------------------------+--------------------+----------"
    fi

    for account in "${ACCOUNTS[@]}"; do
        addr="${account%%:*}"
        pk="${account##*:}"

        balance=$(cast balance "$addr" --rpc-url "$rpc" --ether 2>/dev/null || echo "error")
        balance=$(printf "%.4f" "$balance" 2>/dev/null || echo "$balance")
        nonce=$(cast nonce "$addr" --rpc-url "$rpc" 2>/dev/null || echo "error")

        if [ "$show_roles" = "true" ]; then
            role=$(get_role "$addr")
            printf "%-12s | %-44s | %-68s | %18s | %8s\n" "$role" "$addr" "$pk" "$balance" "$nonce"
        else
            printf "%-44s | %-68s | %18s | %8s\n" "$addr" "$pk" "$balance" "$nonce"
        fi
    done
}

print_table "$L1_RPC" "L1" "true"
print_table "$L2_RPC" "L2" "false"
echo ""
