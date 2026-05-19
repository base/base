#!/usr/bin/env bash
# check-factory-live.sh — end-to-end validation of the B-20 TokenFactory precompile
# against a running devnet node using real cast transactions.
#
# Prerequisites:
#   • Node running at RPC_URL (default: http://localhost:8545)
#   • cast (foundry) in PATH
#
# Usage:
#   ./check-factory-live.sh [rpc-url]
#
# Examples:
#   ./check-factory-live.sh
#   ./check-factory-live.sh http://localhost:8545

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; CYAN='\033[0;36m'; YELLOW='\033[0;33m'; NC='\033[0m'

pass() {
    echo -e "${GREEN}  [PASS] $1${NC}"
    if [[ $# -gt 1 ]]; then shift; echo -e "         $*"; fi
}
fail() {
    echo -e "${RED}  [FAIL] $1${NC}" >&2
    if [[ $# -gt 1 ]]; then shift; echo -e "         $*" >&2; fi
    exit 1
}
section() { echo -e "\n${CYAN}=== $1 ===${NC}"; }
info()    { echo -e "${YELLOW}  →  $1${NC}"; }

# ── Config ────────────────────────────────────────────────────────────────────

# Source devnet accounts if the env file exists
ENV_FILE="$REPO_ROOT/etc/docker/devnet-env"
[[ -f "$ENV_FILE" ]] && source "$ENV_FILE"

RPC_URL="${1:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"

# Pick the first account pair that actually has ETH on this node.
# The devnet genesis may fund different accounts than the standard Anvil set.
ALICE_ADDR=""
ALICE_KEY=""
BOB_ADDR=""

declare -a CANDIDATE_PAIRS=(
    "${ANVIL_ACCOUNT_7_ADDR:-}:${ANVIL_ACCOUNT_7_KEY:-}"
    "${ANVIL_ACCOUNT_2_ADDR:-}:${ANVIL_ACCOUNT_2_KEY:-}"
    "${ANVIL_ACCOUNT_4_ADDR:-}:${ANVIL_ACCOUNT_4_KEY:-}"
    "${ANVIL_ACCOUNT_0_ADDR:-0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266}:${ANVIL_ACCOUNT_0_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
)

for pair in "${CANDIDATE_PAIRS[@]}"; do
    addr="${pair%%:*}"; key="${pair##*:}"
    [[ -z "$addr" || -z "$key" ]] && continue
    bal=$(cast balance --rpc-url "$RPC_URL" "$addr" 2>/dev/null || echo "0")
    # Compare as string: non-zero and not empty means funded
    if [[ -n "$bal" && "$bal" != "0" ]]; then
        ALICE_ADDR="$addr"; ALICE_KEY="$key"; break
    fi
done
[[ -n "$ALICE_ADDR" ]] || { echo "No funded account found — check devnet genesis"; exit 1; }

# Bob: pick a different funded account for the transfer recipient
declare -a BOB_CANDIDATES=(
    "${ANVIL_ACCOUNT_8_ADDR:-}:${ANVIL_ACCOUNT_8_KEY:-}"
    "${ANVIL_ACCOUNT_3_ADDR:-}:${ANVIL_ACCOUNT_3_KEY:-}"
    "${ANVIL_ACCOUNT_1_ADDR:-0x70997970C51812dc3A010C7d01b50e0d17dc79C8}:${ANVIL_ACCOUNT_1_KEY:-}"
)
for pair in "${BOB_CANDIDATES[@]}"; do
    addr="${pair%%:*}"
    [[ -z "$addr" || "$addr" == "$ALICE_ADDR" ]] && continue
    BOB_ADDR="$addr"; break
done
[[ -n "$BOB_ADDR" ]] || BOB_ADDR="0x70997970C51812dc3A010C7d01b50e0d17dc79C8"

# Factory precompile (singleton, fixed at chain genesis)
FACTORY="0xb02f000000000000000000000000000000000000"

# Token creation parameters
TOKEN_NAME="Base USD"
TOKEN_SYMBOL="BUSD"
TOKEN_DECIMALS=6
INITIAL_SUPPLY=1000000          # 1 BUSD (6 decimals → 1.000000)
SUPPLY_CAP=1000000000000        # 1 000 000 BUSD
# Unique salt per run so repeated executions always create a fresh token.
SALT="0x$(cast keccak "check-factory-live-$$-$(date +%s)" | sed 's/0x//')"

# Transfer amount: 300_000 micro-BUSD = 0.3 BUSD
TRANSFER_AMOUNT=300000

# ── Helpers ───────────────────────────────────────────────────────────────────

# Trim whitespace, quotes, and cast's pretty-print suffix (e.g. "1000000 [1e6]" → "1000000")
trim() { echo "$1" | tr -d '"' | sed 's/ \[.*\]$//' | xargs; }

# cast call wrapper — always read-only, does not consume gas
ccall() {
    local addr="$1"; local sig="$2"; shift 2
    cast call --rpc-url "$RPC_URL" "$addr" "$sig" "$@" 2>&1
}


assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        pass "$label" "expected=$expected actual=$actual"
    else
        fail "$label" "expected=$expected  actual=$actual"
    fi
}

# ── 0. Pre-flight ─────────────────────────────────────────────────────────────

section "0/5  Pre-flight checks"

command -v cast >/dev/null 2>&1 || fail "cast not found — install foundry: https://getfoundry.sh"

CHAIN_ID=$(cast chain-id --rpc-url "$RPC_URL" 2>&1) || \
    fail "Node not reachable at $RPC_URL — start the devnet first (just devnet up)"
info "Connected to chain $CHAIN_ID at $RPC_URL"
pass "node is reachable"

ALICE_BAL=$(cast balance --rpc-url "$RPC_URL" "$ALICE_ADDR" 2>&1)
[[ -n "$ALICE_BAL" && "$ALICE_BAL" != "0" ]] || \
    fail "Alice ($ALICE_ADDR) has no ETH — check genesis allocation"
pass "Alice is funded ($ALICE_ADDR)" "balance=$(cast from-wei "$ALICE_BAL") ETH"

# ── 1. Address prediction ─────────────────────────────────────────────────────

section "1/5  Predict token address (read-only)"

PREDICTED=$(ccall "$FACTORY" \
    "predictTokenAddress(address,uint8,uint8,bytes32)(address)" \
    "$ALICE_ADDR" 1 "$TOKEN_DECIMALS" "$SALT") || fail "predictTokenAddress call failed" "$PREDICTED"
PREDICTED=$(trim "$PREDICTED")
[[ "$PREDICTED" =~ ^0x[0-9a-fA-F]{40}$ ]] || \
    fail "predictTokenAddress returned bad address" "$PREDICTED"
info "Predicted token address: $PREDICTED"
pass "predictTokenAddress returned a valid address"

# Verify the prefix encodes the B-20 marker, variant=DEFAULT, and decimals.
PREFIX=$(echo "${PREDICTED:2:8}" | tr '[:upper:]' '[:lower:]')
EXPECTED_PREFIX=$(printf "b02001%02x" "$TOKEN_DECIMALS")
[[ "$PREFIX" == "$EXPECTED_PREFIX" ]] || \
    fail "Token address does not encode DEFAULT variant and decimals" "expected prefix: 0x$EXPECTED_PREFIX got prefix: 0x$PREFIX"
pass "Address prefix encodes B-20 marker, DEFAULT variant, and decimals"

# isB20 must be false before creation (no code yet)
IS_B20_BEFORE=$(ccall "$FACTORY" "isB20(address)(bool)" "$PREDICTED")
IS_B20_BEFORE=$(trim "$IS_B20_BEFORE")
assert_eq "isB20 is false before creation" "false" "$IS_B20_BEFORE"

# ── 2. Create token ───────────────────────────────────────────────────────────

section "2/5  Create token (real transaction)"

# Build B20TokenParams, then pass it as requiredParams into CreateTokenParams.
# B20TokenParams field order: name,symbol,decimals,admin,capabilities,initialSupply,
#                            initialSupplyRecipient,supplyCap,minimumRedeemable,contractURI
REQUIRED_PARAMS=$(cast abi-encode \
    "params(string,string,uint8,address,uint256,uint256,address,uint256,uint256,string)" \
    "$TOKEN_NAME" "$TOKEN_SYMBOL" "$TOKEN_DECIMALS" "$ALICE_ADDR" 3 "$INITIAL_SUPPLY" \
    "$ALICE_ADDR" "$SUPPLY_CAP" 0 "ipfs://check-factory-live")

# CreateTokenParams field order: version,variant,requiredParams,optionalParams,postCreateCalls,salt
PARAMS="(1,1,$REQUIRED_PARAMS,0x,[],$SALT)"

info "Sending createToken transaction …"
TX_OUTPUT=$(cast send \
    --rpc-url "$RPC_URL" \
    --private-key "$ALICE_KEY" \
    --json \
    --confirmations 2 \
    "$FACTORY" \
    "createToken((uint8,uint8,bytes,bytes,bytes[],bytes32))" \
    "$PARAMS") || fail "createToken transaction failed" "$TX_OUTPUT"

TX_HASH=$(echo "$TX_OUTPUT" | grep -o '"transactionHash":"[^"]*"' | cut -d'"' -f4)
TX_STATUS=$(echo "$TX_OUTPUT" | grep -o '"status":"[^"]*"' | cut -d'"' -f4)
[[ "$TX_STATUS" == "0x1" ]] || fail "createToken reverted (status=$TX_STATUS)" "tx=$TX_HASH"
info "Transaction: $TX_HASH  (status=$TX_STATUS)"
pass "createToken transaction mined and succeeded"

# The token address must match the prediction
TOKEN="$PREDICTED"
info "Token deployed at: $TOKEN"

# ── 3. Verify factory state ───────────────────────────────────────────────────

section "3/5  Verify factory state (read-only calls)"

# isB20 must now be true
IS_B20=$(ccall "$FACTORY" "isB20(address)(bool)" "$TOKEN")
IS_B20=$(trim "$IS_B20")
assert_eq "isB20 is true after creation" "true" "$IS_B20"

# variantOf must return 1 (VARIANT_DEFAULT)
VARIANT=$(ccall "$FACTORY" "variantOf(address)(uint8)" "$TOKEN")
VARIANT=$(trim "$VARIANT")
assert_eq "variantOf returns 1 (DEFAULT)" "1" "$VARIANT"

FACTORY_DECIMALS=$(ccall "$FACTORY" "decimalsOf(address)(uint8)" "$TOKEN")
FACTORY_DECIMALS=$(trim "$FACTORY_DECIMALS")
assert_eq "decimalsOf returns encoded decimals" "$TOKEN_DECIMALS" "$FACTORY_DECIMALS"

pass "Factory state is correct"

# ── 4. Verify token metadata ──────────────────────────────────────────────────

section "4/5  Verify token metadata (calls to token address)"

NAME=$(trim "$(ccall "$TOKEN" "name()(string)")")
assert_eq "name()" "$TOKEN_NAME" "$NAME"

SYMBOL=$(trim "$(ccall "$TOKEN" "symbol()(string)")")
assert_eq "symbol()" "$TOKEN_SYMBOL" "$SYMBOL"

DECIMALS=$(trim "$(ccall "$TOKEN" "decimals()(uint8)")")
assert_eq "decimals()" "$TOKEN_DECIMALS" "$DECIMALS"

TOTAL_SUPPLY=$(trim "$(ccall "$TOKEN" "totalSupply()(uint256)")")
assert_eq "totalSupply()" "$INITIAL_SUPPLY" "$TOTAL_SUPPLY"

ALICE_TOKEN_BAL=$(trim "$(ccall "$TOKEN" "balanceOf(address)(uint256)" "$ALICE_ADDR")")
assert_eq "balanceOf(alice) = initialSupply" "$INITIAL_SUPPLY" "$ALICE_TOKEN_BAL"

BOB_TOKEN_BAL=$(trim "$(ccall "$TOKEN" "balanceOf(address)(uint256)" "$BOB_ADDR")")
assert_eq "balanceOf(bob) = 0 before transfer" "0" "$BOB_TOKEN_BAL"

pass "All metadata fields match creation parameters"

# ── 5. Transfer tokens ────────────────────────────────────────────────────────

section "5/5  Transfer tokens (real transaction)"

info "Sending transfer($BOB_ADDR, $TRANSFER_AMOUNT) from Alice …"
XFER_OUTPUT=$(cast send \
    --rpc-url "$RPC_URL" \
    --private-key "$ALICE_KEY" \
    --json \
    --confirmations 2 \
    "$TOKEN" \
    "transfer(address,uint256)" \
    "$BOB_ADDR" "$TRANSFER_AMOUNT") || fail "transfer transaction failed" "$XFER_OUTPUT"

XFER_HASH=$(echo "$XFER_OUTPUT" | grep -o '"transactionHash":"[^"]*"' | cut -d'"' -f4)
XFER_STATUS=$(echo "$XFER_OUTPUT" | grep -o '"status":"[^"]*"' | cut -d'"' -f4)
[[ "$XFER_STATUS" == "0x1" ]] || fail "transfer reverted (status=$XFER_STATUS)" "tx=$XFER_HASH"
info "Transaction: $XFER_HASH  (status=$XFER_STATUS)"
pass "transfer transaction mined and succeeded"

# Verify balances changed correctly
EXPECTED_ALICE=$((INITIAL_SUPPLY - TRANSFER_AMOUNT))
ALICE_BAL_AFTER=$(trim "$(ccall "$TOKEN" "balanceOf(address)(uint256)" "$ALICE_ADDR")")
assert_eq "Alice balance after transfer" "$EXPECTED_ALICE" "$ALICE_BAL_AFTER"

BOB_BAL_AFTER=$(trim "$(ccall "$TOKEN" "balanceOf(address)(uint256)" "$BOB_ADDR")")
assert_eq "Bob balance after transfer" "$TRANSFER_AMOUNT" "$BOB_BAL_AFTER"

# Total supply must be unchanged by a transfer
TOTAL_AFTER=$(trim "$(ccall "$TOKEN" "totalSupply()(uint256)")")
assert_eq "totalSupply unchanged after transfer" "$INITIAL_SUPPLY" "$TOTAL_AFTER"

pass "Balances updated correctly; total supply preserved"

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
echo -e "${GREEN}All live checks passed.${NC}"
echo ""
echo "Token: $TOKEN  (chain $CHAIN_ID, RPC $RPC_URL)"
echo ""
echo "Verified:"
echo "  • predictTokenAddress → deterministic address with B-20 marker, variant, and decimals"
echo "  • isB20 = false before creation, true after"
echo "  • variantOf = 1 (DEFAULT)"
echo "  • decimalsOf = $TOKEN_DECIMALS"
echo "  • name='$TOKEN_NAME'  symbol='$TOKEN_SYMBOL'  decimals=$TOKEN_DECIMALS"
echo "  • totalSupply=$INITIAL_SUPPLY  balanceOf(alice)=$ALICE_TOKEN_BAL"
echo "  • transfer($TRANSFER_AMOUNT to bob) → alice=$EXPECTED_ALICE  bob=$TRANSFER_AMOUNT"
echo "  • totalSupply unchanged after transfer"
