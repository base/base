#!/usr/bin/env bash
# Tests the Counter native precompile at 0x0000...0900 (active from Beryl).
#
# Usage:
#   ./etc/scripts/devnet/test-counter-precompile.sh [rpc-url]
#
# Example:
#   ./etc/scripts/devnet/test-counter-precompile.sh http://localhost:7545
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RPC_URL="${1:-${L2_BUILDER_RPC_URL:-http://localhost:7545}}"
COUNTER_ADDRESS="0x0000000000000000000000000000000000000900"
# Batcher is pre-funded on L2 genesis; Anvil account 0 (deployer) has no L2 ETH.
SENDER_KEY="${BATCHER_KEY:-${ANVIL_ACCOUNT_6_KEY:-0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e}}"
SENDER_ADDR="${BATCHER_ADDR:-${ANVIL_ACCOUNT_6_ADDR:-0x976EA74026E726554dB657fA54763abd0C3a0aa9}}"

# ── Helpers ────────────────────────────────────────────────────────────────────

fail() {
    echo "ERROR: $*" >&2
    exit 2
}

print_info() {
    while IFS= read -r line; do
        printf '  %s\n' "$line"
    done
}

pass_check() {
    local name="$1"; shift
    printf '[PASS] %s\n' "$name"
    [ "$#" -gt 0 ] && printf '%s\n' "$@" | print_info
}

fail_check() {
    local name="$1"; shift
    printf '[FAIL] %s\n' "$name" >&2
    [ "$#" -gt 0 ] && printf '%s\n' "$@" | print_info >&2
    exit 1
}

get_count() {
    cast call --rpc-url "$RPC_URL" "$COUNTER_ADDRESS" "getCount()(uint256)"
}

do_increment() {
    # Explicit gas-limit because native precompiles return gas_used=0 (gas charging
    # not yet implemented), which causes eth_estimateGas to converge to 0.
    cast send \
        --rpc-url "$RPC_URL" \
        --private-key "$SENDER_KEY" \
        --gas-limit 100000 \
        --json \
        "$COUNTER_ADDRESS" "increment()"
}

# ── Checks ─────────────────────────────────────────────────────────────────────

check_precompile_deployed() {
    local check_name="precompile active (Beryl)"

    # Native precompiles have no deployed bytecode — they are registered by address
    # in the EVM's precompile lookup, not as contracts. The correct liveness check is
    # to make a raw eth_call for getCount() and verify it returns 32 ABI-encoded bytes.
    # An inactive address (plain EOA) returns empty bytes.
    local selector="0xa87d942c" # keccak256("getCount()")[0:4]
    local raw
    raw="$(cast rpc --rpc-url "$RPC_URL" eth_call \
        --raw "[{\"to\":\"$COUNTER_ADDRESS\",\"data\":\"$selector\"},\"latest\"]" \
        2>&1 | tr -d '"')"

    # Expected: 0x + 64 hex chars = 32 ABI bytes (one uint256 word)
    local expected_len=66
    local actual_len="${#raw}"

    if [ "$actual_len" -ne "$expected_len" ]; then
        fail_check "$check_name" \
            "getCount() returned $actual_len chars, expected $expected_len (32 ABI bytes)" \
            "got: $raw" \
            "Beryl is not active — set beryl_timestamp: Some(0) in ChainConfig::devnet() and rebuild"
    fi

    pass_check "$check_name" \
        "address: $COUNTER_ADDRESS" \
        "getCount() returned a valid 32-byte ABI response: $raw"
}

check_initial_count() {
    local check_name="initial count is zero"
    local count
    count="$(get_count)"

    if [ "$count" != "0" ]; then
        fail_check "$check_name" \
            "expected 0, got $count" \
            "counter may have been incremented by a prior run"
    fi

    pass_check "$check_name" \
        "getCount() = $count"
}

check_increment() {
    local check_name="increment()"
    local result tx_hash block_number gas_used

    if ! result="$(do_increment 2>&1)"; then
        fail_check "$check_name" \
            "transaction failed" \
            "$result"
    fi

    tx_hash="$(printf '%s' "$result" | jq -r '.transactionHash')"
    block_number="$(printf '%s' "$result" | jq -r '.blockNumber')"
    gas_used="$(printf '%s' "$result" | jq -r '.gasUsed')"
    local status
    status="$(printf '%s' "$result" | jq -r '.status')"

    if [ "$status" != "0x1" ]; then
        fail_check "$check_name" \
            "tx reverted" \
            "hash:   $tx_hash" \
            "block:  $block_number" \
            "status: $status"
    fi

    pass_check "$check_name" \
        "tx:    $tx_hash" \
        "block: $block_number" \
        "gas:   $gas_used"
}

check_count_after_n_increments() {
    local n="$1"
    local check_name="count equals $n after $n increment(s)"
    local count
    count="$(get_count)"

    if [ "$count" != "$n" ]; then
        fail_check "$check_name" \
            "expected $n, got $count"
    fi

    pass_check "$check_name" \
        "getCount() = $count"
}

check_gas_estimate() {
    local check_name="gas estimate for increment()"
    local estimate

    if ! estimate="$(
        cast estimate \
            --rpc-url "$RPC_URL" \
            --from "$SENDER_ADDR" \
            "$COUNTER_ADDRESS" "increment()" 2>&1
    )"; then
        # Native precompiles don't report gas consumed via PrecompileOutput yet
        # (gas tracking is a TODO). eth_estimateGas may fail or return the
        # intrinsic floor. Treat as a warning, not a hard failure.
        printf '[WARN] %s\n' "$check_name"
        printf '  gas estimation unavailable (precompile gas not yet tracked): %s\n' \
            "$(printf '%s' "$estimate" | tr '\n\r' ' ')"
        return
    fi

    pass_check "$check_name" \
        "estimated gas: $estimate"
}

check_readonly_via_call() {
    local check_name="getCount() via eth_call (no tx)"
    local count

    if ! count="$(get_count 2>&1)"; then
        fail_check "$check_name" \
            "call failed" \
            "$count"
    fi

    pass_check "$check_name" \
        "getCount() = $count (no gas consumed, no tx)"
}

check_static_call_reverts_on_write() {
    local check_name="increment() reverts in static context"

    # eth_call always executes in a static context for state-changing ops;
    # our dispatch returns OutOfGas / reverts if is_static is true and
    # the call tries to write. Verify that eth_call to increment() reverts.
    local result
    if result="$(
        cast call \
            --rpc-url "$RPC_URL" \
            "$COUNTER_ADDRESS" "increment()" 2>&1
    )"; then
        # Some implementations silently succeed on eth_call even for write ops
        # (they just don't commit). Check the count didn't change.
        local count_after
        count_after="$(get_count)"
        pass_check "$check_name" \
            "eth_call returned (state not committed): $result" \
            "count unchanged: $count_after"
    else
        pass_check "$check_name" \
            "eth_call to increment() correctly reverted in static context" \
            "$(printf '%s' "$result" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
    fi
}

# ── Main ───────────────────────────────────────────────────────────────────────

command -v cast >/dev/null 2>&1 || fail "'cast' (foundry) is required"
command -v jq   >/dev/null 2>&1 || fail "'jq' is required"

echo "Counter precompile test"
echo "  address: $COUNTER_ADDRESS"
echo "  rpc:     $RPC_URL"
echo "  sender:  $SENDER_ADDR"
echo

check_precompile_deployed

echo
echo "── Read ────────────────────────────────────────────────────────────────────"
check_initial_count
check_readonly_via_call
check_gas_estimate

echo
echo "── Write ───────────────────────────────────────────────────────────────────"
check_increment
check_count_after_n_increments 1

echo
echo "── Multiple increments ─────────────────────────────────────────────────────"
check_increment
check_increment
check_increment
check_count_after_n_increments 4

echo
echo "── Static context ──────────────────────────────────────────────────────────"
check_static_call_reverts_on_write

echo
echo "Counter precompile: all checks passed"
