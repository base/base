#!/usr/bin/env bash
# Demo for gas limit pricing (docs/guides/GAS_LIMIT_PRICING.md).
#
# Sends several plain ETH transfers that each declare a large gas limit but only
# need ~21,000 gas, then shows that on a Zenith devnet the sender is debited for
# the full declared limit: receipt gasUsed == gasLimit, the balance delta equals
# sum(value + gasUsed * effectiveGasPrice + l1Fee), eth_estimateGas still returns
# the metered requirement, and the block's gasUsed is the sum of declared limits.
#
# Usage (from the repository root, after `just devnet up-single zenith`):
#   ./etc/scripts/devnet/gas-limit-pricing-demo.sh [rpc-url]
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
    receipt="$(cast receipt "$hash" --rpc-url "$RPC" --json)"
    receipts="$(jq -c --argjson r "$receipt" '. + [$r]' <<<"$receipts")"
done

BAL_AFTER="$(cast balance "$FROM" --rpc-url "$RPC")"

# Block gasUsed and the declared limits of every transaction in each touched block.
blocks="{}"
for number in $(jq -r '[.[].blockNumber] | unique | .[]' <<<"$receipts"); do
    number="$(cast to-dec "$number")"
    block="$(cast block "$number" --full --rpc-url "$RPC" --json)"
    blocks="$(jq -c --arg n "$number" --argjson b "$block" '. + {($n): $b}' <<<"$blocks")"
done

RECEIPTS="$receipts" BLOCKS="$blocks" python3 - "$BAL_BEFORE" "$BAL_AFTER" "$ESTIMATE" "$GAS_LIMIT" "$VALUE_WEI" <<'PY'
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

print(f"{'tx':<14}{'block':>8}{'status':>8}{'gasUsed':>12}{'effGasPrice':>14}{'l1Fee':>16}{'charged (wei)':>24}")
expected = 0
metered_expected = 0
all_limit = True
for r in receipts:
    gas_used, price, l1 = n(r.get("gasUsed")), n(r.get("effectiveGasPrice")), n(r.get("l1Fee"))
    charged = value + gas_used * price + l1
    expected += charged
    metered_expected += value + estimate * price + l1
    all_limit &= gas_used == gas_limit
    print(f"{r['transactionHash'][:12]:<14}{n(r['blockNumber']):>8}{n(r['status']):>8}{gas_used:>12}{price:>14}{l1:>16}{charged:>24}")

delta = bal_before - bal_after
print()
print(f"receipt gasUsed == declared gas limit ({gas_limit}) for every tx: {'yes' if all_limit else 'NO'}")
print(f"sender balance delta:                         {delta} wei")
print(f"sum(value + gasUsed*effectiveGasPrice + l1Fee): {expected} wei")
residual = delta - expected
if residual == 0:
    print("balance delta matches the receipts exactly (operator fee is zero on this devnet).")
elif residual > 0:
    print(f"balance delta exceeds the receipt sum by {residual} wei (operator fee, charged on gasUsed).")
else:
    print(f"MISMATCH: balance delta is {-residual} wei below the receipt sum.")
print()
print(f"what the same transfers would have cost at metered gas ({estimate} each): {metered_expected} wei")
if metered_expected:
    print(f"gas limit pricing multiplier on the execution fee: {gas_limit / estimate:.1f}x; total paid {expected / metered_expected:.2f}x")

print()
print("block accounting:")
for number, b in sorted(blocks.items(), key=lambda kv: n(kv[0])):
    txs = b.get("transactions", [])
    limits = sum(n(t.get("gas")) for t in txs if isinstance(t, dict))
    print(f"  block {n(number)}: gasUsed={n(b.get('gasUsed'))} sum(declared tx gas limits)={limits} txs={len(txs)}"
          + ("  <- header gasUsed is derivable from the body" if n(b.get("gasUsed")) == limits else "  <- differs (pre-Zenith block or metered deposits)"))

ok = all_limit and residual >= 0 and all(n(r["status"]) == 1 for r in receipts)
print()
print("RESULT: PASS" if ok else "RESULT: FAIL")
sys.exit(0 if ok else 1)
PY
