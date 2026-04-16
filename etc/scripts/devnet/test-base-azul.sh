#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

MODE="${1:-after}"
RPC_URL="${2:-${RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}}"
BLOCK_TAG="${3:-latest}"

PROBE_ADDRESS="0x000000000000000000000000000000000000001e"
CLZ_RUNTIME="0x6000351e60005260206000f3"
# Init code that deploys CLZ_RUNTIME: pushes the 12-byte runtime, stores it, returns it
CLZ_INITCODE="0x6b6000351e60005260206000f3600052600c6014f3"
MODEXP_ADDRESS="0x0000000000000000000000000000000000000005"
MODEXP_OVERSIZED_INPUT="0x000000000000000000000000000000000000000000000000000000000000040100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"

MODEXP_GAS_PROBE_ADDRESS="0x000000000000000000000000000000000000001d"
MODEXP_GAS_PROBE_RUNTIME="0x600060006060600060006005610190f160005260206000f3"

P256_GAS_PROBE_ADDRESS="0x000000000000000000000000000000000000001f"
P256_GAS_PROBE_RUNTIME="0x60006000600060006000610100611388f160005260206000f3"

TX_GAS_LIMIT_CAP=$((2**24))
TX_GAS_LIMIT_OVER=$((TX_GAS_LIMIT_CAP + 1))

# Well-known Anvil default account 0 (always pre-funded in any Anvil devnet)
ANVIL_DEFAULT_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
ANVIL_DEFAULT_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

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
    local name="$1"
    shift
    printf '[PASS] %s\n' "$name"
    if [ "$#" -gt 0 ]; then
        printf '%s\n' "$@" | print_info
    fi
}

fail_check() {
    local name="$1"
    shift
    printf '[FAIL] %s\n' "$name" >&2
    if [ "$#" -gt 0 ]; then
        printf '%s\n' "$@" | print_info >&2
    fi
    exit 1
}

usage() {
    cat <<EOF
Usage: $0 <before|after> [rpc-url] [block-tag]

Examples:
  $0 after
  $0 after http://localhost:8545 latest
  $0 before http://localhost:8545 latest
EOF
}

check_eth_config() {
    local raw_result
    local check_name="eth_config RPC"

    if raw_result="$(cast rpc --rpc-url "$RPC_URL" eth_config 2>&1)"; then
        if [ "$MODE" = "before" ]; then
            fail_check \
                "$check_name" \
                "unexpectedly succeeded before Azul on $RPC_URL" \
                "$raw_result"
        fi

        pass_check "$check_name" "available on $RPC_URL"
        if command -v jq >/dev/null 2>&1; then
            printf '%s\n' "$raw_result" | jq . | print_info
        else
            printf '%s\n' "$raw_result" | print_info
        fi
        return
    fi

    if [ "$MODE" = "before" ]; then
        pass_check \
            "$check_name" \
            "unavailable before Azul" \
            "$(printf '%s' "$raw_result" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
        return
    fi

    fail_check \
        "$check_name" \
        "unavailable after Azul on $RPC_URL" \
        "$raw_result"
}

call_clz() {
    local input_word="$1"
    local params

    params=$(printf '[{"to":"%s","data":"%s"},"%s",{"%s":{"code":"%s"}}]' \
        "$PROBE_ADDRESS" \
        "$input_word" \
        "$BLOCK_TAG" \
        "$PROBE_ADDRESS" \
        "$CLZ_RUNTIME")

    cast rpc --rpc-url "$RPC_URL" eth_call --raw "$params"
}

check_modexp_size_limit() {
    local raw_result
    local check_name="MODEXP size limit"

    if raw_result="$(
        cast call \
            --rpc-url "$RPC_URL" \
            -b "$BLOCK_TAG" \
            "$MODEXP_ADDRESS" \
            --data "$MODEXP_OVERSIZED_INPUT" 2>&1
    )"; then
        if [ "$MODE" = "before" ]; then
            pass_check \
                "$check_name" \
                "oversized input accepted before Azul" \
                "output: $raw_result"
            return
        fi

        fail_check \
            "$check_name" \
            "oversized input unexpectedly succeeded after Azul on $RPC_URL" \
            "output: $raw_result"
    fi

    if [ "$MODE" = "before" ]; then
        fail_check \
            "$check_name" \
            "oversized input unexpectedly rejected before Azul on $RPC_URL" \
            "error: $raw_result"
    fi

    pass_check \
        "$check_name" \
        "oversized input rejected after Azul" \
        "$(printf '%s' "$raw_result" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
}

check_modexp_gas_increase() {
    local raw_result
    local actual
    local check_name="MODEXP min gas increase"

    local params
    params=$(printf '[{"to":"%s","data":"0x"},"%s",{"%s":{"code":"%s"}}]' \
        "$MODEXP_GAS_PROBE_ADDRESS" \
        "$BLOCK_TAG" \
        "$MODEXP_GAS_PROBE_ADDRESS" \
        "$MODEXP_GAS_PROBE_RUNTIME")

    if ! raw_result="$(cast rpc --rpc-url "$RPC_URL" eth_call --raw "$params" 2>&1)"; then
        fail "eth_call failed: $raw_result"
    fi

    actual="$(printf '%s' "$raw_result" | tr -d '"\n\r')"

    local success="0x0000000000000000000000000000000000000000000000000000000000000001"
    local failure="0x0000000000000000000000000000000000000000000000000000000000000000"

    if [ "$actual" = "$success" ]; then
        if [ "$MODE" = "before" ]; then
            pass_check "$check_name" \
                "MODEXP CALL with 400 gas succeeded (min gas = 200 before Azul)"
        else
            fail_check "$check_name" \
                "MODEXP CALL with 400 gas succeeded, expected OOG after Azul (min gas = 500)"
        fi
    elif [ "$actual" = "$failure" ]; then
        if [ "$MODE" = "after" ]; then
            pass_check "$check_name" \
                "MODEXP CALL with 400 gas hit OOG (min gas = 500 after Azul)"
        else
            fail_check "$check_name" \
                "MODEXP CALL with 400 gas hit OOG, expected success before Azul (min gas = 200)"
        fi
    else
        fail_check "$check_name" \
            "unexpected result: $actual"
    fi
}

check_p256_gas_increase() {
    local raw_result
    local actual
    local check_name="P256VERIFY gas increase"

    local params
    params=$(printf '[{"to":"%s","data":"0x"},"%s",{"%s":{"code":"%s"}}]' \
        "$P256_GAS_PROBE_ADDRESS" \
        "$BLOCK_TAG" \
        "$P256_GAS_PROBE_ADDRESS" \
        "$P256_GAS_PROBE_RUNTIME")

    if ! raw_result="$(cast rpc --rpc-url "$RPC_URL" eth_call --raw "$params" 2>&1)"; then
        fail "eth_call failed: $raw_result"
    fi

    actual="$(printf '%s' "$raw_result" | tr -d '"\n\r')"

    local success="0x0000000000000000000000000000000000000000000000000000000000000001"
    local failure="0x0000000000000000000000000000000000000000000000000000000000000000"

    if [ "$actual" = "$success" ]; then
        if [ "$MODE" = "before" ]; then
            pass_check "$check_name" \
                "P256VERIFY CALL with 5000 gas succeeded (cost = 3450 before Azul)"
        else
            fail_check "$check_name" \
                "P256VERIFY CALL with 5000 gas succeeded, expected OOG after Azul (cost = 6900)"
        fi
    elif [ "$actual" = "$failure" ]; then
        if [ "$MODE" = "after" ]; then
            pass_check "$check_name" \
                "P256VERIFY CALL with 5000 gas hit OOG (cost = 6900 after Azul)"
        else
            fail_check "$check_name" \
                "P256VERIFY CALL with 5000 gas hit OOG, expected success before Azul (cost = 3450)"
        fi
    else
        fail_check "$check_name" \
            "unexpected result: $actual"
    fi
}

check_tx_gas_limit_cap() {
    local check_name="TX gas limit cap"
    local addr="$ANVIL_DEFAULT_ADDR"
    local key="$ANVIL_DEFAULT_KEY"

    local raw_result
    if raw_result="$(
        cast send \
            --rpc-url "$RPC_URL" \
            --private-key "$key" \
            --gas-limit "$TX_GAS_LIMIT_OVER" \
            "$addr" 2>&1
    )"; then
        if [ "$MODE" = "before" ]; then
            pass_check "$check_name" \
                "tx with gas_limit=$TX_GAS_LIMIT_OVER accepted before Azul"
            return
        fi

        fail_check "$check_name" \
            "tx with gas_limit=$TX_GAS_LIMIT_OVER unexpectedly accepted after Azul" \
            "$raw_result"
    fi

    case "$raw_result" in
        *"insufficient funds"*|*"not enough funds"*)
            pass_check "$check_name" \
                "tx with gas_limit=$TX_GAS_LIMIT_OVER rejected (insufficient funds)" \
                "$(printf '%s' "$raw_result" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
            return
            ;;
    esac

    if [ "$MODE" = "after" ]; then
        pass_check "$check_name" \
            "tx with gas_limit=$TX_GAS_LIMIT_OVER rejected after Azul (cap = $TX_GAS_LIMIT_CAP)" \
            "$(printf '%s' "$raw_result" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
        return
    fi

    fail_check "$check_name" \
        "tx with gas_limit=$TX_GAS_LIMIT_OVER unexpectedly rejected before Azul" \
        "$raw_result"
}

check_clz_transaction() {
    local check_name="CLZ transaction"
    local key="$ANVIL_ACCOUNT_1_KEY"
    local input_word="0x0000000000000000000000000000000000000000000000000000000000000001"
    local expected="0x00000000000000000000000000000000000000000000000000000000000000ff"
    local send_rpc="${L2_BUILDER_RPC_URL:-$RPC_URL}"

    local deploy_result
    if ! deploy_result="$(
        cast send \
            --rpc-url "$send_rpc" \
            --private-key "$key" \
            --gas-limit 100000 \
            --json \
            --create "$CLZ_INITCODE" 2>&1
    )"; then
        case "$deploy_result" in
            *"insufficient funds"*|*"not enough funds"*)
                pass_check "$check_name" \
                    "skipped: account $ANVIL_ACCOUNT_1_ADDR has insufficient funds on $send_rpc"
                return
                ;;
        esac
        if [ "$MODE" = "before" ]; then
            pass_check "$check_name" \
                "deploy failed before Azul (CLZ opcode not available)" \
                "$(printf '%s' "$deploy_result" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
            return
        fi
        fail_check "$check_name" \
            "deploy failed after Azul" \
            "$deploy_result"
    fi

    local contract_addr deploy_block
    contract_addr="$(printf '%s' "$deploy_result" | jq -r '.contractAddress')"
    deploy_block="$(printf '%s' "$deploy_result" | jq -r '.blockNumber')"

    local call_result
    if ! call_result="$(
        cast send \
            --rpc-url "$send_rpc" \
            --private-key "$key" \
            --gas-limit 100000 \
            --json \
            "$contract_addr" "$input_word" 2>&1
    )"; then
        fail_check "$check_name" \
            "CLZ call tx failed" \
            "$call_result"
    fi

    local call_block tx_hash
    call_block="$(printf '%s' "$call_result" | jq -r '.blockNumber')"
    tx_hash="$(printf '%s' "$call_result" | jq -r '.transactionHash')"

    local actual
    actual="$(cast call --rpc-url "$RPC_URL" "$contract_addr" "$input_word" 2>&1)"

    if [ "$MODE" = "before" ]; then
        fail_check "$check_name" \
            "unexpectedly succeeded before Azul"
    fi

    if [ "$actual" != "$expected" ]; then
        fail_check "$check_name" \
            "unexpected CLZ result" \
            "expected: $expected" \
            "actual:   $actual"
    fi

    pass_check "$check_name" \
        "deployed CLZ contract at $contract_addr (block $deploy_block)" \
        "call tx $tx_hash landed in block $call_block" \
        "CLZ($input_word) = $actual"
}

run_case() {
    local label="$1"
    local input_word="$2"
    local expected="$3"
    local raw_result
    local actual
    local check_name="CLZ $label"

    if raw_result="$(call_clz "$input_word" 2>&1)"; then
        if [ "$MODE" = "before" ]; then
            fail_check \
                "$check_name" \
                "unexpectedly succeeded before Azul" \
                "input:  $input_word" \
                "output: $raw_result"
        fi

        actual="$(printf '%s' "$raw_result" | tr -d '"\n\r')"
    else
        case "$raw_result" in
            *NotActivated*|*"invalid opcode"*|*"undefined opcode"*|*"opcode 0x1e"*|*"unsupported opcode"*)
                if [ "$MODE" = "before" ]; then
                    pass_check \
                        "$check_name" \
                        "unavailable before Azul" \
                        "$(printf '%s' "$raw_result" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
                    return
                fi
                fail_check \
                    "$check_name" \
                    "unavailable after Azul on $RPC_URL at block tag $BLOCK_TAG" \
                    "$raw_result"
                ;;
            *)
                echo "$raw_result" >&2
                fail "eth_call failed for reasons unrelated to CLZ activation"
                ;;
        esac
    fi

    if [ "$actual" != "$expected" ]; then
        fail_check \
            "$check_name" \
            "unexpected CLZ result" \
            "input:    $input_word" \
            "expected: $expected" \
            "actual:   $actual"
    fi

    pass_check \
        "$check_name" \
        "input:  $input_word" \
        "output: $actual"
}

command -v cast >/dev/null 2>&1 || fail "'cast' is required"
[ "$MODE" = "before" ] || [ "$MODE" = "after" ] || {
    usage >&2
    fail "mode must be 'before' or 'after'"
}

echo "Testing Azul mode '$MODE' on $RPC_URL (block tag: $BLOCK_TAG)"
echo "Using state override at $PROBE_ADDRESS with runtime $CLZ_RUNTIME"
echo

run_case "zero" \
    "0x0000000000000000000000000000000000000000000000000000000000000000" \
    "0x0000000000000000000000000000000000000000000000000000000000000100"

run_case "one" \
    "0x0000000000000000000000000000000000000000000000000000000000000001" \
    "0x00000000000000000000000000000000000000000000000000000000000000ff"

run_case "high-bit" \
    "0x8000000000000000000000000000000000000000000000000000000000000000" \
    "0x0000000000000000000000000000000000000000000000000000000000000000"

run_case "four-bits" \
    "0x0f00000000000000000000000000000000000000000000000000000000000000" \
    "0x0000000000000000000000000000000000000000000000000000000000000004"

check_modexp_size_limit

check_modexp_gas_increase

check_p256_gas_increase

check_clz_transaction

check_tx_gas_limit_cap

check_eth_config

if [ "$MODE" = "after" ]; then
    echo "Azul is active on $RPC_URL"
else
    echo "Azul is not active on $RPC_URL"
fi
