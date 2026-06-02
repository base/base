#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

source "$SCRIPT_DIR/common.sh"

CONTRACT_ROOT="$REPO_ROOT/crates/utilities/test-utils/contracts"
ENV_OUT="$REPO_ROOT/.devnet/upgrade-signal.env"
L1_RPC="${UPGRADE_SIGNAL_L1_RPC_URL:-${L1_RPC_URL:-http://localhost:4545}}"
L2_RPC="${UPGRADE_SIGNAL_L2_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
HARDFORK_ID="${UPGRADE_SIGNAL_HARDFORK_ID:-azul}"
PROTOCOL_VERSION="${UPGRADE_SIGNAL_PROTOCOL_VERSION:-7}"
ACTIVATION_OFFSET="${UPGRADE_SIGNAL_ACTIVATION_OFFSET:-120}"

require_cmd() {
    local name="$1"
    if ! command -v "$name" >/dev/null 2>&1; then
        echo "missing required command: $name" >&2
        exit 1
    fi
}

require_cmd cast
require_cmd forge
require_cmd jq

echo "Checking devnet RPCs..."
cast block-number --rpc-url "$L1_RPC" >/dev/null

if [[ -n "${UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP:-}" ]]; then
    if ! [[ "$UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP" =~ ^[0-9]+$ ]]; then
        echo "UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP must be a non-negative integer" >&2
        exit 1
    fi
    L2_TIMESTAMP="${UPGRADE_SIGNAL_REFERENCE_TIMESTAMP:-not-read}"
    REFERENCE_TIMESTAMP_LABEL="reference timestamp"
    ACTIVATION_TIMESTAMP="$UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP"
else
    L2_BLOCK_JSON="$(cast rpc --rpc-url "$L2_RPC" eth_getBlockByNumber latest false)"
    L2_TIMESTAMP_HEX="$(jq -r '.timestamp' <<< "$L2_BLOCK_JSON")"
    if [[ "$L2_TIMESTAMP_HEX" == "null" || -z "$L2_TIMESTAMP_HEX" ]]; then
        echo "failed to read latest L2 timestamp from $L2_RPC" >&2
        exit 1
    fi

    L2_TIMESTAMP="$((16#${L2_TIMESTAMP_HEX#0x}))"
    REFERENCE_TIMESTAMP_LABEL="latest L2 timestamp"
    ACTIVATION_TIMESTAMP="$((L2_TIMESTAMP + ACTIVATION_OFFSET))"
fi

echo "Deploying MockUpgradeSignal to $L1_RPC..."
DEPLOY_JSON="$(
    forge create \
        --root "$CONTRACT_ROOT" \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        --broadcast \
        src/MockUpgradeSignal.sol:MockUpgradeSignal \
        --json
)"
CONTRACT_ADDRESS="$(
    jq -r '
        .deployedTo //
        .contractAddress //
        .address //
        .receipts[0].contractAddress //
        .transactions[0].contractAddress //
        empty
    ' <<< "$DEPLOY_JSON"
)"
if [[ -z "$CONTRACT_ADDRESS" ]]; then
    echo "failed to parse deployed contract address" >&2
    echo "$DEPLOY_JSON" >&2
    exit 1
fi

echo "Setting hardfork=$HARDFORK_ID timestamp=$ACTIVATION_TIMESTAMP protocol_version=$PROTOCOL_VERSION..."
cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    "$CONTRACT_ADDRESS" \
    "setTimestamp(string,uint256)" \
    "$HARDFORK_ID" \
    "$ACTIVATION_TIMESTAMP" \
    --json >/dev/null

cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    "$CONTRACT_ADDRESS" \
    "setProtocolVersion(string,uint256)" \
    "$HARDFORK_ID" \
    "$PROTOCOL_VERSION" \
    --json >/dev/null

READ_TIMESTAMP="$(cast call --rpc-url "$L1_RPC" "$CONTRACT_ADDRESS" "getTimestamp(string)(uint256)" "$HARDFORK_ID")"
READ_VERSION="$(cast call --rpc-url "$L1_RPC" "$CONTRACT_ADDRESS" "getProtocolVersion(string)(uint256)" "$HARDFORK_ID")"

mkdir -p "$(dirname "$ENV_OUT")"
cat > "$ENV_OUT" <<EOF
UPGRADE_SIGNAL_CONTRACT=$CONTRACT_ADDRESS
UPGRADE_SIGNAL_HARDFORK_ID=$HARDFORK_ID
UPGRADE_SIGNAL_PROTOCOL_VERSION=$PROTOCOL_VERSION
UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP=$ACTIVATION_TIMESTAMP
L2_BASE_AZUL_BLOCK=
L2_BASE_BERYL_BLOCK=
EOF

cat <<EOF

Mock upgrade signal configured.

contract:              $CONTRACT_ADDRESS
hardfork id:           $HARDFORK_ID
$REFERENCE_TIMESTAMP_LABEL:   $L2_TIMESTAMP
activation timestamp:  $ACTIVATION_TIMESTAMP
protocol version:      $PROTOCOL_VERSION
contract timestamp:    $READ_TIMESTAMP
contract version:      $READ_VERSION
env file:              $ENV_OUT

Restart the observer services with:
  docker compose --env-file etc/docker/devnet-env --env-file .devnet/upgrade-signal.env -f etc/docker/docker-compose.yml -f etc/docker/docker-compose.upgrade-signal.yml up -d --no-build --no-deps --force-recreate base-builder base-builder-cl base-client base-client-cl base-rpc

Watch logs with:
  docker compose --env-file etc/docker/devnet-env --env-file .devnet/upgrade-signal.env -f etc/docker/docker-compose.yml -f etc/docker/docker-compose.upgrade-signal.yml logs -f base-builder base-builder-cl base-client base-client-cl base-rpc

Check metrics with:
  curl -s http://localhost:7300/metrics | grep upgrade_signal
  curl -s http://localhost:8090/metrics | grep upgrade_signal
  curl -s http://localhost:8300/metrics | grep upgrade_signal
EOF
