#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

source "$SCRIPT_DIR/common.sh"

ACTIVATION_OFFSET="${UPGRADE_SIGNAL_ACTIVATION_OFFSET:-120}"
POLL_INTERVAL="${UPGRADE_SIGNAL_TEST_POLL_INTERVAL:-2}"
TIMEOUT_SECONDS="${UPGRADE_SIGNAL_TEST_TIMEOUT_SECONDS:-300}"
CHECKPOINT_ENV="$REPO_ROOT/.devnet/upgrade-signal-checkpoint.env"
UPGRADE_ENV="$REPO_ROOT/.devnet/upgrade-signal.env"
L1_RPC="${UPGRADE_SIGNAL_L1_RPC_URL:-${L1_RPC_URL:-http://localhost:4545}}"
L2_RPC="${UPGRADE_SIGNAL_L2_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
ROLLUP_JSON="$REPO_ROOT/.devnet/l2/configs/rollup.json"
GENESIS_JSON="$REPO_ROOT/.devnet/l2/configs/genesis.json"

BASE_COMPOSE=(
    docker compose
    --env-file etc/docker/devnet-env
    --env-file "$CHECKPOINT_ENV"
    -f etc/docker/docker-compose.yml
)
UPGRADE_COMPOSE=(
    docker compose
    --env-file etc/docker/devnet-env
    --env-file "$CHECKPOINT_ENV"
    --env-file "$UPGRADE_ENV"
    -f etc/docker/docker-compose.yml
    -f etc/docker/docker-compose.upgrade-signal.yml
)

require_cmd() {
    local name="$1"
    if ! command -v "$name" >/dev/null 2>&1; then
        echo "missing required command: $name" >&2
        exit 1
    fi
}

compose_base() {
    (cd "$REPO_ROOT" && "${BASE_COMPOSE[@]}" "$@")
}

compose_upgrade() {
    (cd "$REPO_ROOT" && "${UPGRADE_COMPOSE[@]}" "$@")
}

reset_devnet() {
    echo "Resetting Docker devnet state..."
    export L2_BASE_AZUL_BLOCK=
    export L2_BASE_BERYL_BLOCK=

    (
        cd "$REPO_ROOT"
        docker compose \
            --env-file etc/docker/devnet-env \
            -f etc/docker/docker-compose.yml \
            -f etc/docker/docker-compose.ha.yml \
            --profile profiling \
            down
    )

    mkdir -p "$REPO_ROOT/.devnet"
    find "$REPO_ROOT/.devnet" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
    cat > "$CHECKPOINT_ENV" <<EOF
L2_BASE_AZUL_BLOCK=
L2_BASE_BERYL_BLOCK=
EOF
}

wait_for_l1_rpc() {
    local deadline=$((SECONDS + TIMEOUT_SECONDS))

    echo "Waiting for L1 RPC at $L1_RPC..."
    until cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1; do
        if ((SECONDS >= deadline)); then
            echo "timed out waiting for L1 RPC at $L1_RPC" >&2
            exit 1
        fi
        sleep "$POLL_INTERVAL"
    done
}

wait_for_l2_rpc() {
    local deadline=$((SECONDS + TIMEOUT_SECONDS))

    echo "Waiting for L2 RPC at $L2_RPC..."
    until cast block-number --rpc-url "$L2_RPC" >/dev/null 2>&1; do
        if ((SECONDS >= deadline)); then
            echo "timed out waiting for L2 RPC at $L2_RPC" >&2
            exit 1
        fi
        sleep "$POLL_INTERVAL"
    done
}

latest_l2_timestamp() {
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

wait_for_activation_timestamp() {
    local activation_timestamp="$1"
    local deadline=$((SECONDS + TIMEOUT_SECONDS))
    local current

    echo "Waiting for L2 timestamp to reach $activation_timestamp..."
    while true; do
        current="$(latest_l2_timestamp)"
        if ((current >= activation_timestamp)); then
            echo "L2 timestamp reached activation: $current"
            return
        fi
        if ((SECONDS >= deadline)); then
            echo "timed out waiting for activation timestamp; latest L2 timestamp is $current" >&2
            exit 1
        fi
        sleep "$POLL_INTERVAL"
    done
}

require_cmd cast
require_cmd docker
require_cmd forge
require_cmd jq

if ! [[ "$ACTIVATION_OFFSET" =~ ^[0-9]+$ ]]; then
    echo "UPGRADE_SIGNAL_ACTIVATION_OFFSET must be a non-negative integer" >&2
    exit 1
fi

reset_devnet

echo "Starting L1 services..."
compose_base up -d --no-build setup-l1 l1-el l1-cl l1-vc
wait_for_l1_rpc

echo "Generating L2 configs with static Azul and Beryl disabled..."
compose_base up --no-build setup-l2

if [[ ! -f "$ROLLUP_JSON" || ! -f "$GENESIS_JSON" ]]; then
    echo "expected L2 configs were not generated" >&2
    exit 1
fi
if jq -e '.base.azul? // empty' "$ROLLUP_JSON" >/dev/null; then
    echo "rollup config still contains static base.azul" >&2
    exit 1
fi
if jq -e '.config.osakaTime? // empty' "$GENESIS_JSON" >/dev/null; then
    echo "genesis config still contains static osakaTime" >&2
    exit 1
fi

L2_GENESIS_TIME="$(jq -re '.genesis.l2_time' "$ROLLUP_JSON")"
ACTIVATION_TIMESTAMP="$((L2_GENESIS_TIME + ACTIVATION_OFFSET))"

echo "Deploying upgrade signal contract for Azul at timestamp $ACTIVATION_TIMESTAMP..."
UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP="$ACTIVATION_TIMESTAMP" \
    UPGRADE_SIGNAL_REFERENCE_TIMESTAMP="$L2_GENESIS_TIME" \
    UPGRADE_SIGNAL_L1_RPC_URL="$L1_RPC" \
    "$SCRIPT_DIR/setup-upgrade-signal.sh"

echo "Starting L2 services with upgrade signal enabled..."
compose_upgrade up -d --no-build \
    base-el-bootnode \
    base-cl-bootnode \
    base-builder \
    base-builder-cl \
    base-batcher \
    base-client \
    base-client-cl
wait_for_l2_rpc

CURRENT_TIMESTAMP="$(latest_l2_timestamp)"
if ((CURRENT_TIMESTAMP >= ACTIVATION_TIMESTAMP)); then
    echo "missed pre-activation window: latest L2 timestamp $CURRENT_TIMESTAMP >= $ACTIVATION_TIMESTAMP" >&2
    exit 1
fi

echo "Running pre-Azul checks at L2 timestamp $CURRENT_TIMESTAMP..."
"$SCRIPT_DIR/test-base-azul.sh" before "$L2_RPC" latest

wait_for_activation_timestamp "$ACTIVATION_TIMESTAMP"

echo "Running post-Azul checks..."
"$SCRIPT_DIR/test-base-azul.sh" after "$L2_RPC" latest

cat <<EOF

Contract-driven upgrade signal devnet test passed.

L2 genesis timestamp:     $L2_GENESIS_TIME
Activation offset:        $ACTIVATION_OFFSET seconds
Contract Azul timestamp:  $ACTIVATION_TIMESTAMP
EOF
