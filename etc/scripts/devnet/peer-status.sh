#!/usr/bin/env bash
set -eo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

set -u

rpc() {
    local url="$1"
    local method="$2"
    local timeout="${RPC_TIMEOUT_SECONDS:-2}"

    curl -fsS \
        --max-time "$timeout" \
        -X POST \
        -H "Content-Type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":[],\"id\":1}" \
        "$url" 2>/dev/null
}

json_field() {
    local response="$1"
    local field="$2"

    jq -r "$field // \"N/A\"" <<<"$response" 2>/dev/null || echo "N/A"
}

hex_to_dec() {
    local hex="$1"

    if [[ "$hex" =~ ^0x[0-9a-fA-F]+$ ]]; then
        printf "%d" "$((16#${hex#0x}))"
    else
        echo "$hex"
    fi
}

print_node() {
    local name="$1"
    local el_rpc="$2"
    local cl_rpc="$3"

    local peer_response peer_count_hex peer_count
    if peer_response="$(rpc "$el_rpc" "net_peerCount")"; then
        peer_count_hex="$(json_field "$peer_response" ".result")"
        peer_count="$(hex_to_dec "$peer_count_hex")"
    else
        peer_count_hex="N/A"
        peer_count="N/A"
    fi

    local sync_response unsafe_height
    if sync_response="$(rpc "$cl_rpc" "optimism_syncStatus")"; then
        unsafe_height="$(json_field "$sync_response" ".result.unsafe_l2.number")"
    else
        unsafe_height="N/A"
    fi

    printf "%-14s | %-22s | %-22s | %-5s | %-10s | %-10s\n" \
        "$name" "$el_rpc" "$cl_rpc" "$peer_count" "$peer_count_hex" "$unsafe_height"
}

printf "\n"
printf "%-14s | %-22s | %-22s | %-5s | %-10s | %-10s\n" \
    "Node" "EL RPC" "CL RPC" "Peers" "Peers Hex" "Unsafe"
printf "%-14s-+-%-22s-+-%-22s-+-%-5s-+-%-10s-+-%-10s\n" \
    "--------------" "----------------------" "----------------------" "-----" "----------" "----------"

print_node "Builder" "$L2_BUILDER_RPC_URL" "$L2_BUILDER_OP_RPC_URL"
print_node "Client" "$L2_CLIENT_RPC_URL" "$L2_CLIENT_OP_RPC_URL"
print_node "Sequencer 1" "http://localhost:${L2_SEQ1_HTTP_PORT}" "http://localhost:${L2_SEQ1_CL_RPC_PORT}"
print_node "Sequencer 2" "http://localhost:${L2_SEQ2_HTTP_PORT}" "http://localhost:${L2_SEQ2_CL_RPC_PORT}"

printf "\n"
