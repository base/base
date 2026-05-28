#!/usr/bin/env bash

# Shared network defaults for activation registry scripts.

ACTIVATION_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ACTIVATION_ENV_FILE="$ACTIVATION_SCRIPT_DIR/../../docker/devnet-env"

if [[ -f "$ACTIVATION_ENV_FILE" ]]; then
    set -a
    source "$ACTIVATION_ENV_FILE"
    set +a
fi

# Feature id ↔ canonical name pairs (kept in sync with
# crates/common/precompiles/src/activation/storage.rs::ActivationFeature).
ACTIVATION_FEATURES=(
    "base.b20_token:0x47a1afe8d3d691b87e090ee972d223a11f4da971ff5416c04985bb2393aca752"
    "base.b20_factory:0x78751e29c8bcc0d609ab18e9fbc4158e73f7db25ae2ee095dad42e2578b1e800"
    "base.policy_registry:0xb582ebae03f16fee49a6763f78df482fb11ae73f103ed0d330bbe556aa90a43f"
    "base.b20_stablecoin:0xecfa0def2c10020caaf65e6155aa69c84b24892aaef76eeac52e0e2b3a0b8601"
    "base.b20_security:0x83d32fab502ae0e8bc4352a117767262cb5e47cc8d67a744008ed4ff03fcf5e6"
)

activation_supported_networks() {
    echo "vibes, devnet, base, base-sepolia, base-zeronet, custom"
}

activation_normalize_network() {
    local network="$1"
    network="$(echo "$network" | tr '[:upper:]_' '[:lower:]-')"

    case "$network" in
        "" | vibes | vibenet)
            echo "vibes"
            ;;
        dev | devnet | local)
            echo "devnet"
            ;;
        base | mainnet | base-mainnet)
            echo "base"
            ;;
        sepolia | base-sepolia)
            echo "base-sepolia"
            ;;
        zeronet | base-zeronet)
            echo "base-zeronet"
            ;;
        custom)
            echo "custom"
            ;;
        *)
            echo "Unknown network '$1'. Supported networks: $(activation_supported_networks)" >&2
            return 1
            ;;
    esac
}

activation_arg_is_url() {
    [[ "$1" == http://* || "$1" == https://* ]]
}

activation_trim() {
    local value="$1"
    value="${value//\"/}"
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    value="${value% \[*\]}"
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    printf '%s\n' "$value"
}

resolve_activation_network_defaults() {
    local hardhat_account_5_addr="${ANVIL_ACCOUNT_5_ADDR:-0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc}"
    local hardhat_account_5_key="${ANVIL_ACCOUNT_5_KEY:-0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba}"

    NETWORK="$(activation_normalize_network "${NETWORK:-${ACTIVATION_NETWORK:-vibes}}")" || return 1
    RPC_URL="${RPC_URL:-${ACTIVATION_RPC_URL:-}}"
    ADMIN_ADDR="${ADMIN_ADDR:-${ACTIVATION_ADMIN_ADDR:-}}"
    ADMIN_KEY="${ADMIN_KEY:-${ACTIVATION_ADMIN_KEY:-}}"

    case "$NETWORK" in
        vibes)
            RPC_URL="${RPC_URL:-${VIBES_RPC_URL:-https://rpc.vibes.base.org}}"
            ADMIN_ADDR="${ADMIN_ADDR:-${VIBES_ACTIVATION_ADMIN_ADDR:-$hardhat_account_5_addr}}"
            ADMIN_KEY="${ADMIN_KEY:-${VIBES_ACTIVATION_ADMIN_KEY:-}}"
            ;;
        devnet)
            RPC_URL="${RPC_URL:-${DEVNET_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}}"
            ADMIN_ADDR="${ADMIN_ADDR:-${DEVNET_ACTIVATION_ADMIN_ADDR:-${L2_ACTIVATION_ADMIN_ADDR:-$hardhat_account_5_addr}}}"
            ADMIN_KEY="${ADMIN_KEY:-${DEVNET_ACTIVATION_ADMIN_KEY:-${L2_ACTIVATION_ADMIN_KEY:-$hardhat_account_5_key}}}"
            ;;
        base)
            RPC_URL="${RPC_URL:-${BASE_MAINNET_RPC_URL:-${BASE_RPC_URL:-}}}"
            ADMIN_ADDR="${ADMIN_ADDR:-${BASE_MAINNET_ACTIVATION_ADMIN_ADDR:-0x331C9d37BbcebBC9dfAf98FBE3C5B8A39Dd6E771}}"
            ADMIN_KEY="${ADMIN_KEY:-${BASE_MAINNET_ACTIVATION_ADMIN_KEY:-}}"
            ;;
        base-sepolia)
            RPC_URL="${RPC_URL:-${BASE_SEPOLIA_RPC_URL:-}}"
            ADMIN_ADDR="${ADMIN_ADDR:-${BASE_SEPOLIA_ACTIVATION_ADMIN_ADDR:-0x5Be7Dd3678e999D5F7bC508c413db239F7D4Ac59}}"
            ADMIN_KEY="${ADMIN_KEY:-${BASE_SEPOLIA_ACTIVATION_ADMIN_KEY:-}}"
            ;;
        base-zeronet)
            RPC_URL="${RPC_URL:-${BASE_ZERONET_RPC_URL:-}}"
            ADMIN_ADDR="${ADMIN_ADDR:-${BASE_ZERONET_ACTIVATION_ADMIN_ADDR:-0xF5969A85a555671EeD766C4ff0C61426AA626b11}}"
            ADMIN_KEY="${ADMIN_KEY:-${BASE_ZERONET_ACTIVATION_ADMIN_KEY:-}}"
            ;;
        custom)
            RPC_URL="${RPC_URL:-${CUSTOM_RPC_URL:-}}"
            ADMIN_ADDR="${ADMIN_ADDR:-${CUSTOM_ACTIVATION_ADMIN_ADDR:-}}"
            ADMIN_KEY="${ADMIN_KEY:-${CUSTOM_ACTIVATION_ADMIN_KEY:-}}"
            ;;
    esac
}

require_activation_rpc_url() {
    if [[ -n "${RPC_URL:-}" ]]; then
        return 0
    fi

    echo "No RPC URL configured for network '$NETWORK'." >&2
    echo "Pass --rpc-url <url> or set ACTIVATION_RPC_URL / a network-specific RPC env var." >&2
    return 1
}
