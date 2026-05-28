#!/usr/bin/env bash
# check-activations.sh — query the activation registry precompile and print the
# activation state of every known Base-native feature.
#
# Prerequisites:
#   • cast (foundry) in PATH
#
# Usage:
#   ./check-activations.sh [network] [rpc-url]
#   ./check-activations.sh --network <network> [--rpc-url <url>]
#
# Examples:
#   ./check-activations.sh
#   ./check-activations.sh vibes
#   ./check-activations.sh devnet http://localhost:8545

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/activation-networks.sh"
NETWORK="${ACTIVATION_NETWORK:-vibes}"

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; CYAN='\033[0;36m'; YELLOW='\033[0;33m'; BOLD='\033[1m'; NC='\033[0m'

# ── Config ────────────────────────────────────────────────────────────────────
REGISTRY="0x8453000000000000000000000000000000000001"

# Feature id ↔ canonical name pairs (kept in sync with
# crates/common/precompiles/src/activation/storage.rs::ActivationFeature).
FEATURES=(
    "base.b20_token:0x47a1afe8d3d691b87e090ee972d223a11f4da971ff5416c04985bb2393aca752"
    "base.b20_factory:0x78751e29c8bcc0d609ab18e9fbc4158e73f7db25ae2ee095dad42e2578b1e800"
    "base.policy_registry:0xb582ebae03f16fee49a6763f78df482fb11ae73f103ed0d330bbe556aa90a43f"
    "base.b20_stablecoin:0xecfa0def2c10020caaf65e6155aa69c84b24892aaef76eeac52e0e2b3a0b8601"
    "base.b20_security:0x83d32fab502ae0e8bc4352a117767262cb5e47cc8d67a744008ed4ff03fcf5e6"
)

# ── Helpers ───────────────────────────────────────────────────────────────────
trim() { echo "$1" | tr -d '"' | sed 's/ \[.*\]$//' | xargs; }

usage() {
    cat <<EOF
Usage:
  $0 [network] [rpc-url]
  $0 --network <network> [--rpc-url <url>]

Networks: $(activation_supported_networks)

Examples:
  $0
  $0 vibes
  $0 devnet http://localhost:8545
  $0 --network custom --rpc-url https://example.invalid
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
echo -e "${CYAN}${BOLD}Activation Registry${NC} @ ${REGISTRY}"
echo -e "${YELLOW}  → Network: ${NETWORK}${NC}"
echo -e "${YELLOW}  → RPC:     ${RPC_URL}${NC}"

CHAIN_ID=$(cast chain-id --rpc-url "$RPC_URL" 2>&1) || {
    echo -e "${RED}Node not reachable at ${RPC_URL}${NC}" >&2
    exit 1
}
echo -e "${YELLOW}  → Chain ID: ${CHAIN_ID}${NC}"

# admin() — surfaces who can activate/deactivate features on this chain.
ADMIN_RAW=$(cast call --rpc-url "$RPC_URL" "$REGISTRY" "admin()(address)" 2>&1) \
    || { echo -e "${RED}admin() call failed:${NC} $ADMIN_RAW" >&2; exit 1; }
ADMIN=$(trim "$ADMIN_RAW")
echo -e "${YELLOW}  → Admin: ${ADMIN}${NC}"
echo ""

# ── Query each feature ────────────────────────────────────────────────────────
printf "${BOLD}  %-24s  %-66s  %s${NC}\n" "FEATURE" "ID" "STATUS"
printf "  %-24s  %-66s  %s\n" "------------------------" \
    "------------------------------------------------------------------" \
    "----------"

active_count=0
inactive_count=0
total=${#FEATURES[@]}

for entry in "${FEATURES[@]}"; do
    name="${entry%%:*}"
    id="${entry##*:}"

    result=$(cast call --rpc-url "$RPC_URL" "$REGISTRY" \
        "isActivated(bytes32)(bool)" "$id" 2>&1) || {
        printf "  %-24s  %-66s  ${RED}ERROR${NC}  %s\n" "$name" "$id" "$result"
        continue
    }
    result=$(trim "$result")

    if [[ "$result" == "true" ]]; then
        printf "  %-24s  %-66s  ${GREEN}✓ ACTIVE${NC}\n" "$name" "$id"
        active_count=$((active_count + 1))
    else
        printf "  %-24s  %-66s  ${RED}✗ INACTIVE${NC}\n" "$name" "$id"
        inactive_count=$((inactive_count + 1))
    fi
done

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
if [[ "$active_count" -eq "$total" ]]; then
    echo -e "${GREEN}${BOLD}All ${total} features are activated.${NC}"
    exit 0
else
    echo -e "${YELLOW}${BOLD}${active_count}/${total} features activated${NC}" \
        "(${inactive_count} inactive)"
    exit 1
fi
