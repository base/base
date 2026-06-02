#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

source "$SCRIPT_DIR/common.sh"

CONTRACT_ROOT="$REPO_ROOT/crates/utilities/test-utils/contracts"
ENV_OUT="$REPO_ROOT/.devnet/upgrade-signal.env"
ROLLUP_JSON="$REPO_ROOT/.devnet/l2/configs/rollup.json"
L1_RPC="${UPGRADE_SIGNAL_L1_RPC_URL:-${L1_RPC_URL:-http://localhost:4545}}"
L2_RPC="${UPGRADE_SIGNAL_L2_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
HARDFORK_ID="${UPGRADE_SIGNAL_HARDFORK_ID:-azul}"
ACTIVE_HARDFORK_IDS="${UPGRADE_SIGNAL_ACTIVE_HARDFORK_IDS:-regolith,canyon,delta,ecotone,fjord,granite,holocene,isthmus,jovian}"
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

read_latest_l2_timestamp() {
    local block_json
    local timestamp_hex

    block_json="$(cast rpc --rpc-url "$L2_RPC" eth_getBlockByNumber latest false)"
    timestamp_hex="$(jq -r '.timestamp' <<< "$block_json")"
    if [[ "$timestamp_hex" == "null" || -z "$timestamp_hex" ]]; then
        echo "failed to read latest L2 timestamp from $L2_RPC" >&2
        exit 1
    fi

    printf '%d\n' "$((16#${timestamp_hex#0x}))"
}

read_reference_timestamp() {
    if [[ -n "${UPGRADE_SIGNAL_REFERENCE_TIMESTAMP:-}" ]]; then
        if ! [[ "$UPGRADE_SIGNAL_REFERENCE_TIMESTAMP" =~ ^[0-9]+$ ]]; then
            echo "UPGRADE_SIGNAL_REFERENCE_TIMESTAMP must be a non-negative integer" >&2
            exit 1
        fi
        printf '%s\n' "$UPGRADE_SIGNAL_REFERENCE_TIMESTAMP"
        return
    fi

    if [[ -f "$ROLLUP_JSON" ]]; then
        jq -re '.genesis.l2_time' "$ROLLUP_JSON"
        return
    fi

    echo "UPGRADE_SIGNAL_REFERENCE_TIMESTAMP must be set when $ROLLUP_JSON is unavailable" >&2
    exit 1
}

if [[ -n "${UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP:-}" ]]; then
    if ! [[ "$UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP" =~ ^[0-9]+$ ]]; then
        echo "UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP must be a non-negative integer" >&2
        exit 1
    fi
    ACTIVATION_TIMESTAMP="$UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP"
else
    LATEST_L2_TIMESTAMP="$(read_latest_l2_timestamp)"
    ACTIVATION_TIMESTAMP="$((LATEST_L2_TIMESTAMP + ACTIVATION_OFFSET))"
fi

if [[ -n "$ACTIVE_HARDFORK_IDS" ]]; then
    if [[ -n "${UPGRADE_SIGNAL_REFERENCE_TIMESTAMP:-}" ]]; then
        REFERENCE_TIMESTAMP_LABEL="reference timestamp"
    elif [[ -f "$ROLLUP_JSON" ]]; then
        REFERENCE_TIMESTAMP_LABEL="rollup genesis timestamp"
    else
        REFERENCE_TIMESTAMP_LABEL="reference timestamp"
    fi
    L2_TIMESTAMP="$(read_reference_timestamp)"
    if ! [[ "$L2_TIMESTAMP" =~ ^[0-9]+$ ]]; then
        echo "$REFERENCE_TIMESTAMP_LABEL must be a non-negative integer" >&2
        exit 1
    fi
else
    L2_TIMESTAMP="not-set"
    REFERENCE_TIMESTAMP_LABEL="reference timestamp"
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

set_signal() {
    local hardfork_id="$1"
    local timestamp="$2"

    cast send \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        "$CONTRACT_ADDRESS" \
        "setTimestamp(string,uint256)" \
        "$hardfork_id" \
        "$timestamp" \
        --json >/dev/null

    cast send \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        "$CONTRACT_ADDRESS" \
        "setProtocolVersion(string,uint256)" \
        "$hardfork_id" \
        "$PROTOCOL_VERSION" \
        --json >/dev/null
}

if [[ -n "$ACTIVE_HARDFORK_IDS" ]]; then
    IFS=',' read -r -a ACTIVE_HARDFORK_ID_ARRAY <<< "$ACTIVE_HARDFORK_IDS"
    echo "Setting already-active hardforks timestamp=$L2_TIMESTAMP protocol_version=$PROTOCOL_VERSION..."
    for active_hardfork_id in "${ACTIVE_HARDFORK_ID_ARRAY[@]}"; do
        if [[ -z "$active_hardfork_id" ]]; then
            continue
        fi
        set_signal "$active_hardfork_id" "$L2_TIMESTAMP"
    done
fi

echo "Setting target hardfork=$HARDFORK_ID timestamp=$ACTIVATION_TIMESTAMP protocol_version=$PROTOCOL_VERSION..."
set_signal "$HARDFORK_ID" "$ACTIVATION_TIMESTAMP"

READ_TIMESTAMP="$(cast call --rpc-url "$L1_RPC" "$CONTRACT_ADDRESS" "getTimestamp(string)(uint256)" "$HARDFORK_ID")"
READ_VERSION="$(cast call --rpc-url "$L1_RPC" "$CONTRACT_ADDRESS" "getProtocolVersion(string)(uint256)" "$HARDFORK_ID")"

mkdir -p "$(dirname "$ENV_OUT")"
cat > "$ENV_OUT" <<EOF
UPGRADE_SIGNAL_CONTRACT=$CONTRACT_ADDRESS
UPGRADE_SIGNAL_PROTOCOL_VERSION=$PROTOCOL_VERSION
UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP=$ACTIVATION_TIMESTAMP
L2_BASE_AZUL_BLOCK=
L2_BASE_BERYL_BLOCK=
EOF

cat <<EOF

Mock upgrade signal configured.

contract:              $CONTRACT_ADDRESS
active hardfork ids:   $ACTIVE_HARDFORK_IDS
target hardfork id:    $HARDFORK_ID
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
