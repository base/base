#!/usr/bin/env bash
set -euo pipefail

# Test meteredPriorityFeePerGas on local devnet.
#
# Prerequisites:
#   cd etc/docker && docker compose up -d
#
# Usage:
#   ./etc/scripts/devnet/test-metered-priority-fee.sh [phase]
#
# Phases:
#   baseline   Verify uncongested estimates return the default fee
#   gas        Drive gas congestion and check the estimator response
#   poll       Poll the estimator every 2s (run alongside load)
#   all        Run baseline, then gas

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

CLIENT_RPC="${L2_CLIENT_RPC_URL:-http://localhost:8545}"
BUILDER_RPC="${L2_BUILDER_RPC_URL:-http://localhost:7545}"

TEST_KEY="$ANVIL_ACCOUNT_3_KEY"
TEST_ADDR="$ANVIL_ACCOUNT_3_ADDR"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log() { echo -e "${CYAN}[$(date +%H:%M:%S)]${NC} $*"; }
ok() { echo -e "${GREEN}[ok]${NC} $*"; }
warn() { echo -e "${YELLOW}[warn]${NC} $*"; }
fail() { echo -e "${RED}[fail]${NC} $*"; }

rpc_call() {
    local url="$1" method="$2" params="$3"
    curl -s -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}" \
        "$url"
}

get_block_number() {
    local result
    result=$(rpc_call "$CLIENT_RPC" "eth_blockNumber" "[]")
    echo "$result" | jq -r '.result' | xargs printf '%d\n'
}

get_nonce() {
    local addr="$1"
    local result
    result=$(rpc_call "$CLIENT_RPC" "eth_getTransactionCount" "[\"$addr\", \"pending\"]")
    echo "$result" | jq -r '.result' | xargs printf '%d\n'
}

get_chain_id() {
    local result
    result=$(rpc_call "$CLIENT_RPC" "eth_chainId" "[]")
    echo "$result" | jq -r '.result' | xargs printf '%d\n'
}

sign_tx() {
    local to="$1" value="$2" gas_limit="$3" nonce="$4" data="${5:-}" priority_fee="${6:-1000000}"
    local chain_id
    chain_id=$(get_chain_id)

    local cmd=(
        cast mktx
        --private-key "$TEST_KEY"
        "$to"
        --value "$value"
        --gas-limit "$gas_limit"
        --nonce "$nonce"
        --priority-gas-price "$priority_fee"
        --gas-price 1000000000
        --chain "$chain_id"
    )

    if [ -n "$data" ]; then
        cmd+=(--input "$data")
    fi

    "${cmd[@]}" 2>/dev/null
}

build_bundle_json() {
    local txs=("$@")
    local block_num target_block target_hex txs_json first

    block_num=$(get_block_number)
    target_block=$((block_num + 1))
    target_hex=$(printf '0x%x' "$target_block")

    txs_json="["
    first=true
    for tx in "${txs[@]}"; do
        if [ "$first" = true ]; then
            first=false
        else
            txs_json+=","
        fi
        txs_json+="\"$tx\""
    done
    txs_json+="]"

    printf '{"txs":%s,"blockNumber":"%s"}' "$txs_json" "$target_hex"
}

call_metered_priority_fee() {
    local rpc_url="$1"
    shift
    local bundle
    bundle=$(build_bundle_json "$@")
    rpc_call "$rpc_url" "base_meteredPriorityFeePerGas" "[$bundle]"
}

call_meter_bundle() {
    local rpc_url="$1"
    shift
    local bundle
    bundle=$(build_bundle_json "$@")
    rpc_call "$rpc_url" "base_meterBundle" "[$bundle]"
}

print_estimate() {
    local response="$1"
    local error result priority_fee blocks_sampled gas_used exec_time_us fee_dec binding

    error=$(echo "$response" | jq -r '.error // empty')
    if [ -n "$error" ]; then
        fail "RPC error: $(echo "$response" | jq -r '.error.message')"
        return 1
    fi

    result=$(echo "$response" | jq '.result')
    priority_fee=$(echo "$result" | jq -r '.priorityFee')
    blocks_sampled=$(echo "$result" | jq -r '.blocksSampled')
    gas_used=$(echo "$result" | jq -r '.totalGasUsed')
    exec_time_us=$(echo "$result" | jq -r '.totalExecutionTimeUs')
    fee_dec=$(printf '%d' "$priority_fee" 2>/dev/null || echo "$priority_fee")

    echo -e "  ${CYAN}priorityFee${NC}: $fee_dec wei ($priority_fee)"
    echo -e "  ${CYAN}blocksSampled${NC}: $blocks_sampled"
    echo -e "  ${CYAN}bundleGasUsed${NC}: $gas_used"
    echo -e "  ${CYAN}bundleExecTime${NC}: ${exec_time_us}us"
    echo -e "  ${CYAN}resources${NC}:"
    echo "$result" | jq -r '.resourceEstimates[] |
        "    \(.resource): threshold=\(.thresholdPriorityFee) recommended=\(.recommendedPriorityFee) txCount=\(.thresholdTxCount)/\(.totalTransactions)"'

    binding=$(echo "$result" | jq -r '
        .resourceEstimates | max_by(
            .recommendedPriorityFee | if startswith("0x") then (ltrimstr("0x") | explode | map(
                if . >= 97 then . - 87 elif . >= 65 then . - 55 else . - 48 end
            ) | reduce .[] as $d (0; . * 16 + $d)) else (. | tonumber) end
        ) | .resource')
    echo -e "  ${YELLOW}binding constraint${NC}: $binding"
}

phase_baseline() {
    log "Phase 1: Baseline - uncongested estimates"
    log "Waiting for metering cache to warm (12 blocks)..."

    local block_num=0
    for _ in $(seq 1 30); do
        block_num=$(get_block_number 2>/dev/null || echo 0)
        if [ "$block_num" -gt 12 ]; then
            break
        fi
        sleep 2
    done

    if [ "$block_num" -le 12 ]; then
        fail "Devnet not ready or blocks too low ($block_num). Is docker compose running?"
        return 1
    fi
    ok "Devnet at block $block_num"

    log "Building simple transfer bundle (21K gas)..."
    local nonce tx_bytes response meter_response error da_response
    nonce=$(get_nonce "$TEST_ADDR")
    tx_bytes=$(sign_tx "0x000000000000000000000000000000000000dEaD" "1" "21000" "$nonce")

    log "Calling base_meteredPriorityFeePerGas on client (8545)..."
    response=$(call_metered_priority_fee "$CLIENT_RPC" "$tx_bytes")
    print_estimate "$response"
    echo ""

    log "Calling base_meteredPriorityFeePerGas on builder (7545)..."
    response=$(call_metered_priority_fee "$BUILDER_RPC" "$tx_bytes")
    print_estimate "$response"
    echo ""

    log "Calling base_meterBundle on client..."
    meter_response=$(call_meter_bundle "$CLIENT_RPC" "$tx_bytes")
    error=$(echo "$meter_response" | jq -r '.error // empty')
    if [ -n "$error" ]; then
        fail "meterBundle error: $(echo "$meter_response" | jq -r '.error.message')"
    else
        echo "$meter_response" | jq '.result | {totalGasUsed, totalExecutionTimeUs, stateRootTimeUs, stateBlockNumber, stateFlashblockIndex}'
    fi

    log "Calling miner_getMaxDASize on builder..."
    da_response=$(rpc_call "$BUILDER_RPC" "miner_getMaxDASize" "[]")
    echo "$da_response" | jq '.result'
}

phase_poll() {
    log "Polling base_meteredPriorityFeePerGas every 2 seconds..."
    log "Press Ctrl+C to stop. Run load generators in another terminal."
    echo ""

    local nonce tx_bytes response block_num
    nonce=$(get_nonce "$TEST_ADDR")
    tx_bytes=$(sign_tx "0x000000000000000000000000000000000000dEaD" "1" "21000" "$nonce")

    while true; do
        block_num=$(get_block_number 2>/dev/null || echo "?")
        echo -e "${CYAN}[$(date +%H:%M:%S)] block=$block_num${NC}"

        response=$(call_metered_priority_fee "$CLIENT_RPC" "$tx_bytes" 2>/dev/null || true)
        if [ -n "$response" ]; then
            print_estimate "$response" 2>/dev/null || true
        else
            warn "No response"
        fi
        echo ""
        sleep 2
    done
}

phase_gas() {
    log "Phase 2: Gas congestion"
    log "Sending high-gas-price transfers to pressure block capacity..."

    local chain_id nonce i priority
    chain_id=$(get_chain_id)
    nonce=$(get_nonce "$TEST_ADDR")

    for i in $(seq 1 50); do
        priority=$((1000000 + i * 100000))
        cast send --private-key "$TEST_KEY" \
            "0x000000000000000000000000000000000000dEaD" \
            --value 1 \
            --gas-limit 21000 \
            --nonce "$nonce" \
            --priority-gas-price "$priority" \
            --gas-price 1000000000 \
            --chain "$chain_id" \
            --rpc-url "$CLIENT_RPC" \
            --async 2>/dev/null &&
            ok "tx $i (nonce=$nonce, priority=$priority)" ||
            fail "tx $i failed"
        nonce=$((nonce + 1))
    done

    log "Waiting 5 seconds for inclusion..."
    sleep 5

    local probe_tx response
    probe_tx=$(sign_tx "0x000000000000000000000000000000000000dEaD" "1" "21000" "$nonce")

    log "Checking priority fee estimate after gas load..."
    response=$(call_metered_priority_fee "$CLIENT_RPC" "$probe_tx")
    print_estimate "$response"
}

usage() {
    echo "Usage: $0 [phase]"
    echo ""
    echo "Phases:"
    echo "  baseline   Verify uncongested estimates return the default fee"
    echo "  gas        Drive gas congestion"
    echo "  poll       Poll the estimator every 2s (run alongside load)"
    echo "  all        Run baseline, then gas"
    echo ""
    echo "Prerequisites:"
    echo "  cd etc/docker && docker compose up -d"
}

PHASE="${1:-baseline}"

case "$PHASE" in
    baseline) phase_baseline ;;
    gas) phase_gas ;;
    poll) phase_poll ;;
    all)
        phase_baseline
        echo ""
        phase_gas
        ;;
    -h|--help) usage ;;
    *)
        fail "Unknown phase: $PHASE"
        usage
        exit 1
        ;;
esac
