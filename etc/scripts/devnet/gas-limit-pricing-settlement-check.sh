#!/usr/bin/env bash
# Settlement check for gas limit pricing (docs/guides/GAS_LIMIT_PRICING.md). The end-to-end
# demo (reverting-arb flood against the builder's predicate budget) is gas-limit-pricing-demo.sh.
#
# Models a padded transaction (the shape used to game tip-per-gas ordering): several
# plain ETH transfers that each declare a large gas limit but only need ~21,000 gas.
# On a Zenith devnet the sender is debited for the full declared limit: receipt
# gasUsed == gasLimit, the balance delta equals
# sum(value + gasUsed * effectiveGasPrice + l1Fee), eth_estimateGas still returns
# the metered requirement, and each block's gasUsed counts the declared limits of
# its user transactions (deposits stay metered). It also prints what the same
# transfers would have cost at metered gas: the cost multiplier padding now carries.
#
# Usage (from the repository root, after `just devnet up-single zenith`):
#   ./etc/scripts/devnet/gas-limit-pricing-settlement-check.sh [rpc-url]
#
# Environment overrides:
#   TX_COUNT   number of transactions to send            (default 5)
#   GAS_LIMIT  declared gas limit per transaction        (default 1000000)
#   SENDER_KEY private key of the funded devnet sender   (default ANVIL_ACCOUNT_3_KEY)
#
# Requires Foundry's `cast`, `jq`, and `python3`.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RPC="${1:-$L2_BUILDER_RPC_URL}"
TX_COUNT="${TX_COUNT:-5}"
GAS_LIMIT="${GAS_LIMIT:-1000000}"
PK="${SENDER_KEY:-$ANVIL_ACCOUNT_3_KEY}"
TO="$ANVIL_ACCOUNT_4_ADDR"
VALUE_WEI=1
ZENITH_BLOCK="${L2_BASE_ZENITH_BLOCK:-100}"

for tool in cast jq python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: $tool is required" >&2; exit 1; }
done

# Newer Foundry versions wrap `--json` output in a `{data, errors, ...}` envelope.
unwrap() { jq -c 'if type == "object" and has("data") and has("schema_version") then .data else . end'; }

FROM="$(cast wallet address --private-key "$PK")"
CHAIN_ID="$(cast chain-id --rpc-url "$RPC")"

echo "=== Gas limit pricing demo ==="
echo "rpc=$RPC chain_id=$CHAIN_ID sender=$FROM recipient=$TO"
echo "txs=$TX_COUNT declared_gas_limit=$GAS_LIMIT value=${VALUE_WEI}wei"
echo ""

head="$(cast block-number --rpc-url "$RPC")"
if [ "$head" -lt "$ZENITH_BLOCK" ]; then
    echo "Zenith (gas limit pricing) activates at block $ZENITH_BLOCK; head is $head."
    echo "Waiting for activation (set L2_BASE_ZENITH_BLOCK if the devnet uses another block)..."
    while [ "$(cast block-number --rpc-url "$RPC")" -lt "$ZENITH_BLOCK" ]; do sleep 1; done
    echo "Zenith active."
    echo ""
fi

ESTIMATE="$(cast estimate --from "$FROM" "$TO" --value "$VALUE_WEI" --rpc-url "$RPC")"
echo "eth_estimateGas for one transfer: $ESTIMATE gas (metered requirement, unchanged by the rule)"

BAL_BEFORE="$(cast balance "$FROM" --rpc-url "$RPC")"
NONCE="$(cast nonce "$FROM" --rpc-url "$RPC")"
echo "sender balance before: $BAL_BEFORE wei (nonce $NONCE)"
echo ""

echo "Sending $TX_COUNT transfers with --gas-limit $GAS_LIMIT ..."
hashes=()
for ((i = 0; i < TX_COUNT; i++)); do
    hash="$(cast send "$TO" --value "$VALUE_WEI" --gas-limit "$GAS_LIMIT" --nonce "$((NONCE + i))" \
        --private-key "$PK" --rpc-url "$RPC" --chain-id "$CHAIN_ID" --async)"
    hashes+=("$hash")
    echo "  sent $hash"
done
echo ""

echo "Waiting for receipts..."
receipts="[]"
for hash in "${hashes[@]}"; do
    receipt="$(cast receipt "$hash" --rpc-url "$RPC" --json | unwrap)"
    receipts="$(jq -c --argjson r "$receipt" '. + [$r]' <<<"$receipts")"
done

BAL_AFTER="$(cast balance "$FROM" --rpc-url "$RPC")"

# Block gasUsed, the declared limits of the user transactions, and the metered gas of the
# deposits in each touched block (deposits are not priced by gas limit).
blocks="{}"
deposit_receipts="{}"
for number in $(jq -r '[.[].blockNumber] | unique | .[]' <<<"$receipts"); do
    number="$(cast to-dec "$number")"
    block="$(cast block "$number" --full --rpc-url "$RPC" --json | unwrap)"
    blocks="$(jq -c --arg n "$number" --argjson b "$block" '. + {($n): $b}' <<<"$blocks")"
    for hash in $(jq -r '.transactions[] | select(.type == "0x7e") | .hash' <<<"$block"); do
        receipt="$(cast receipt "$hash" --rpc-url "$RPC" --json | unwrap)"
        deposit_receipts="$(jq -c --arg h "$hash" --argjson r "$receipt" '. + {($h): $r}' <<<"$deposit_receipts")"
    done
done

RECEIPTS="$receipts" BLOCKS="$blocks" DEPOSIT_RECEIPTS="$deposit_receipts" python3 - "$BAL_BEFORE" "$BAL_AFTER" "$ESTIMATE" "$GAS_LIMIT" "$VALUE_WEI" <<'PY'
import json, os, sys

def n(x):
    if x is None:
        return 0
    if isinstance(x, int):
        return x
    s = str(x)
    return int(s, 16) if s.startswith("0x") else int(s)

bal_before, bal_after, estimate, gas_limit, value = (n(a) for a in sys.argv[1:6])
receipts = json.loads(os.environ["RECEIPTS"])
blocks = json.loads(os.environ["BLOCKS"])
deposit_receipts = json.loads(os.environ["DEPOSIT_RECEIPTS"])

# Operator fee (Isthmus onward) on the billed gas, from the receipt's L1Block parameters:
# Jovian prices it as gas * scalar * 100 + constant (OPERATOR_FEE_JOVIAN_MULTIPLIER).
def operator_fee(r, gas):
    scalar, constant = n(r.get("operatorFeeScalar")), n(r.get("operatorFeeConstant"))
    return gas * scalar * 100 + constant

print(f"{'tx':<14}{'block':>8}{'status':>8}{'gasUsed':>12}{'effGasPrice':>14}{'l1Fee':>16}{'operatorFee':>14}{'charged (wei)':>24}")
expected = 0
metered_expected = 0
all_limit = True
for r in receipts:
    gas_used, price, l1 = n(r.get("gasUsed")), n(r.get("effectiveGasPrice")), n(r.get("l1Fee"))
    op_fee = operator_fee(r, gas_used)
    charged = value + gas_used * price + l1 + op_fee
    expected += charged
    metered_expected += value + estimate * price + l1 + operator_fee(r, estimate)
    all_limit &= gas_used == gas_limit
    print(f"{r['transactionHash'][:12]:<14}{n(r['blockNumber']):>8}{n(r['status']):>8}{gas_used:>12}{price:>14}{l1:>16}{op_fee:>14}{charged:>24}")

delta = bal_before - bal_after
print()
print(f"receipt gasUsed == declared gas limit ({gas_limit}) for every tx: {'yes' if all_limit else 'NO'}")
print(f"sender balance delta:                                       {delta} wei")
print(f"sum(value + gasUsed*effectiveGasPrice + l1Fee + operatorFee): {expected} wei")
balance_ok = delta == expected
if balance_ok:
    print("balance delta matches the receipts exactly.")
else:
    print(f"MISMATCH: balance delta differs from the receipt sum by {delta - expected} wei.")

# eth_estimateGas must still find the metered requirement: a plain transfer is 21,000 gas
# (the EIP-7623 floor cannot raise a calldata-free call), far below the padded limit.
estimate_ok = estimate == 21_000
print(f"eth_estimateGas is the metered 21,000, not the declared limit: {'yes' if estimate_ok else 'NO (' + str(estimate) + ')'}")
print()
print(f"what the same transfers would have cost at metered gas ({estimate} each): {metered_expected} wei")
if metered_expected:
    print(f"gas limit pricing multiplier on the execution fee: {gas_limit / estimate:.1f}x; total paid {expected / metered_expected:.2f}x")

print()
print("block accounting:")
blocks_ok = True
for number, b in sorted(blocks.items(), key=lambda kv: n(kv[0])):
    txs = [t for t in b.get("transactions", []) if isinstance(t, dict)]
    # Deposits (type 0x7e) are not priced by gas limit and keep metered gas.
    user_limits = sum(n(t.get("gas")) for t in txs if n(t.get("type")) != 0x7E)
    deposit_gas = sum(n(deposit_receipts.get(t["hash"], {}).get("gasUsed")) for t in txs if n(t.get("type")) == 0x7E)
    header = n(b.get("gasUsed"))
    block_ok = header == user_limits + deposit_gas
    blocks_ok &= block_ok
    print(f"  block {n(number)}: gasUsed={header} sum(user tx gas limits)={user_limits} deposit gasUsed={deposit_gas} txs={len(txs)}"
          + ("  <- header counts the reserved capacity" if block_ok else "  <- MISMATCH (pre-Zenith block?)"))

ok = all_limit and balance_ok and estimate_ok and blocks_ok and all(n(r["status"]) == 1 for r in receipts)
print()
print("RESULT: PASS" if ok else "RESULT: FAIL")
sys.exit(0 if ok else 1)
PY
