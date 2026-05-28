#!/usr/bin/env bash
# activate-features.sh — activate every Base-native feature in the activation
# registry precompile.
#
# Prerequisites:
#   • cast (foundry) in PATH
#
# Usage:
#   ./activate-features.sh [network] [rpc-url]
#   ./activate-features.sh --network <network> [--rpc-url <url>]
#
# Examples:
#   ./activate-features.sh
#   ./activate-features.sh vibes
#   ./activate-features.sh devnet http://localhost:8545

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/activation-networks.sh"
NETWORK="${ACTIVATION_NETWORK:-vibes}"

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; CYAN='\033[0;36m'; YELLOW='\033[0;33m'; BOLD='\033[1m'; NC='\033[0m'

# ── Config ────────────────────────────────────────────────────────────────────
REGISTRY="0x8453000000000000000000000000000000000001"

usage() {
    cat <<EOF
Usage:
  $0 [network] [rpc-url]
  $0 --network <network> [--rpc-url <url>] [--admin-addr <address>]

Networks: $(activation_supported_networks)

Examples:
  $0
  $0 vibes
  $0 devnet http://localhost:8545
  ACTIVATION_ADMIN_KEY=<private-key> $0 --network base-sepolia --rpc-url <url>
EOF
}

NETWORK_SET=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        -n | --network)
            shift
            [[ $# -gt 0 ]] || { echo "--network requires a value" >&2; exit 1; }
            NETWORK="$1"
            NETWORK_SET=true
            ;;
        --network=*)
            NETWORK="${1#*=}"
            NETWORK_SET=true
            ;;
        -r | --rpc-url)
            shift
            [[ $# -gt 0 ]] || { echo "--rpc-url requires a value" >&2; exit 1; }
            RPC_URL="$1"
            ;;
        --rpc-url=*)
            RPC_URL="${1#*=}"
            ;;
        --admin-addr)
            shift
            [[ $# -gt 0 ]] || { echo "--admin-addr requires a value" >&2; exit 1; }
            ADMIN_ADDR="$1"
            ;;
        --admin-addr=*)
            ADMIN_ADDR="${1#*=}"
            ;;
        *)
            if activation_arg_is_url "$1"; then
                [[ -z "${RPC_URL:-}" ]] || { echo "RPC URL specified more than once" >&2; exit 1; }
                RPC_URL="$1"
            elif [[ "$NETWORK_SET" == false ]]; then
                NETWORK="$1"
                NETWORK_SET=true
            else
                echo "Unexpected argument: $1" >&2
                usage >&2
                exit 1
            fi
            ;;
    esac
    shift
done

resolve_activation_network_defaults
require_activation_rpc_url

command -v cast >/dev/null 2>&1 || {
    echo -e "${RED}cast not found — install foundry: https://getfoundry.sh${NC}" >&2
    exit 1
}

# ── Pre-flight ────────────────────────────────────────────────────────────────
echo -e "${CYAN}${BOLD}Activate Features${NC} on ${REGISTRY}"
echo -e "${YELLOW}  → Network: ${NETWORK}${NC}"
echo -e "${YELLOW}  → RPC:     ${RPC_URL}${NC}"

CHAIN_ID=$(cast chain-id --rpc-url "$RPC_URL" 2>&1) || {
    echo -e "${RED}Node not reachable at ${RPC_URL}${NC}" >&2
    exit 1
}
echo -e "${YELLOW}  → Chain:   ${CHAIN_ID}${NC}"

ON_CHAIN_ADMIN_RAW=$(cast call --rpc-url "$RPC_URL" "$REGISTRY" "admin()(address)" 2>&1) || {
    echo -e "${RED}admin() call failed:${NC} $ON_CHAIN_ADMIN_RAW" >&2
    exit 1
}
ON_CHAIN_ADMIN=$(activation_trim "$ON_CHAIN_ADMIN_RAW")
ADMIN_ADDR="${ADMIN_ADDR:-$ON_CHAIN_ADMIN}"
echo -e "${YELLOW}  → Admin:   ${ADMIN_ADDR} (on-chain: ${ON_CHAIN_ADMIN})${NC}"

if [[ "$(echo "$ON_CHAIN_ADMIN" | tr '[:upper:]' '[:lower:]')" != "$(echo "$ADMIN_ADDR" | tr '[:upper:]' '[:lower:]')" ]]; then
    echo -e "${RED}Configured admin for ${NETWORK} (${ADMIN_ADDR}) does not match on-chain admin (${ON_CHAIN_ADMIN}).${NC}" >&2
    exit 1
fi

if [[ -z "${ADMIN_KEY:-}" ]]; then
    echo -e "${RED}No activation admin private key configured for network '${NETWORK}'.${NC}" >&2
    echo -e "${RED}Set ACTIVATION_ADMIN_KEY or a network-specific admin key env var.${NC}" >&2
    exit 1
fi

BAL=$(cast balance --rpc-url "$RPC_URL" "$ADMIN_ADDR" 2>&1) || {
    echo -e "${RED}Failed to query admin balance on ${RPC_URL}: ${BAL}${NC}" >&2
    exit 1
}
[[ -n "$BAL" && "$BAL" != "0" ]] || {
    echo -e "${RED}Admin (${ADMIN_ADDR}) has no ETH on ${RPC_URL}${NC}" >&2
    exit 1
}
echo -e "${YELLOW}  → Admin balance: $(cast from-wei "$BAL") ETH${NC}"
echo ""

# ── Activate each feature ─────────────────────────────────────────────────────
activated=0
skipped=0
failed=0

for entry in "${ACTIVATION_FEATURES[@]}"; do
    name="${entry%%:*}"
    id="${entry##*:}"

    current_raw=$(cast call --rpc-url "$RPC_URL" "$REGISTRY" \
        "isActivated(bytes32)(bool)" "$id" 2>&1) || {
        echo -e "  ${RED}✗ fail${NC}  ${name}: isActivated call failed: ${current_raw}"
        failed=$((failed + 1))
        continue
    }
    current=$(activation_trim "$current_raw")

    if [[ "$current" == "true" ]]; then
        echo -e "  ${YELLOW}↷ skip${NC}  ${BOLD}${name}${NC} — already active"
        skipped=$((skipped + 1))
        continue
    fi

    echo -e "  ${CYAN}→ send${NC}  activate(${name}) …"
    out=$(ETH_PRIVATE_KEY="$ADMIN_KEY" cast send \
        --rpc-url "$RPC_URL" \
        --json \
        --confirmations 1 \
        "$REGISTRY" \
        "activate(bytes32)" "$id" 2>&1) || {
        echo -e "  ${RED}✗ fail${NC}  ${name}: ${out}"
        failed=$((failed + 1))
        continue
    }

    tx_hash=$(echo "$out" | grep -o '"transactionHash":"[^"]*"' | cut -d'"' -f4)
    status=$(echo "$out" | grep -o '"status":"[^"]*"' | cut -d'"' -f4)

    if [[ "$status" == "0x1" ]]; then
        echo -e "  ${GREEN}✓ ok${NC}    ${BOLD}${name}${NC}  tx=${tx_hash}"
        activated=$((activated + 1))
    else
        echo -e "  ${RED}✗ fail${NC}  ${name} reverted (status=${status})  tx=${tx_hash}"
        failed=$((failed + 1))
    fi
done

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}Summary:${NC} ${GREEN}${activated} activated${NC}, ${YELLOW}${skipped} skipped${NC}, ${RED}${failed} failed${NC}"

if [[ "$failed" -gt 0 ]]; then
    exit 1
fi
