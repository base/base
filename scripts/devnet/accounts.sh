#!/usr/bin/env bash
set -e

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

L1_RPC="${1:-$L1_RPC_URL}"
L2_RPC="${2:-$L2_CLIENT_RPC_URL}"

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
