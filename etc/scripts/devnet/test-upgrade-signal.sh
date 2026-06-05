#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

source "$SCRIPT_DIR/common.sh"

ACTIVATION_OFFSET="${UPGRADE_SIGNAL_ACTIVATION_OFFSET:-240}"
RESCHEDULE_BEFORE_SECONDS="${UPGRADE_SIGNAL_TEST_RESCHEDULE_BEFORE_SECONDS:-60}"
RESCHEDULE_DELAY_SECONDS="${UPGRADE_SIGNAL_TEST_RESCHEDULE_DELAY_SECONDS:-120}"
POLL_INTERVAL="${UPGRADE_SIGNAL_TEST_POLL_INTERVAL:-2}"
TIMEOUT_SECONDS="${UPGRADE_SIGNAL_TEST_TIMEOUT_SECONDS:-300}"
CHECKPOINT_ENV="$REPO_ROOT/.devnet/upgrade-signal-checkpoint.env"
UPGRADE_ENV="$REPO_ROOT/.devnet/upgrade-signal.env"
L1_RPC="${UPGRADE_SIGNAL_L1_RPC_URL:-${L1_RPC_URL:-http://localhost:4545}}"
L2_RPC="${UPGRADE_SIGNAL_L2_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
L2_BUILDER_RPC="${UPGRADE_SIGNAL_L2_BUILDER_RPC_URL:-${L2_BUILDER_RPC_URL:-http://localhost:7545}}"
L2_BUILDER_OP_RPC="${UPGRADE_SIGNAL_L2_BUILDER_OP_RPC_URL:-${L2_BUILDER_OP_RPC_URL:-http://localhost:7549}}"
L2_CLIENT_OP_RPC="${UPGRADE_SIGNAL_L2_CLIENT_OP_RPC_URL:-${L2_CLIENT_OP_RPC_URL:-http://localhost:8549}}"
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
    export L2_DYNAMIC_HARDFORKS=1

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
L2_DYNAMIC_HARDFORKS=1
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

wait_for_l2_timestamp() {
    local target_timestamp="$1"
    local label="${2:-target timestamp}"
    local deadline=$((SECONDS + TIMEOUT_SECONDS))
    local current

    echo "Waiting for L2 timestamp to reach $target_timestamp ($label)..."
    while true; do
        current="$(latest_l2_timestamp)"
        if ((current >= target_timestamp)); then
            echo "L2 timestamp reached $label: $current"
            return
        fi
        if ((SECONDS >= deadline)); then
            echo "timed out waiting for $label; latest L2 timestamp is $current" >&2
            exit 1
        fi
        sleep "$POLL_INTERVAL"
    done
}

wait_for_activation_timestamp() {
    local activation_timestamp="$1"

    wait_for_l2_timestamp "$activation_timestamp" "activation"
}

upgrade_signal_contract() {
    local contract_address

    if [[ ! -f "$UPGRADE_ENV" ]]; then
        echo "missing upgrade signal env file: $UPGRADE_ENV" >&2
        exit 1
    fi

    contract_address="$(sed -n 's/^UPGRADE_SIGNAL_CONTRACT=//p' "$UPGRADE_ENV" | tail -n 1)"
    if [[ -z "$contract_address" ]]; then
        echo "failed to read UPGRADE_SIGNAL_CONTRACT from $UPGRADE_ENV" >&2
        exit 1
    fi

    printf '%s\n' "$contract_address"
}

set_azul_activation_timestamp() {
    local activation_timestamp="$1"
    local contract_address
    local read_timestamp

    contract_address="$(upgrade_signal_contract)"

    echo "Setting contract Azul timestamp to $activation_timestamp..."
    cast send \
        --rpc-url "$L1_RPC" \
        --private-key "$DEPLOYER_KEY" \
        "$contract_address" \
        "setTimestamp(string,uint256)" \
        "azul" \
        "$activation_timestamp" \
        --json >/dev/null

    read_timestamp="$(
        cast call \
            --rpc-url "$L1_RPC" \
            "$contract_address" \
            "getTimestamp(string)(uint256)" \
            "azul"
    )"
    echo "Contract Azul timestamp is now $read_timestamp"
}

refresh_upgrade_signal() {
    local label="$1"
    local rpc_url="$2"
    local raw_result

    echo "Refreshing upgrade signal on $label..."
    if ! raw_result="$(cast rpc --rpc-url "$rpc_url" admin_refreshUpgradeSignal 2>&1)"; then
        echo "failed to refresh upgrade signal on $label at $rpc_url" >&2
        echo "$raw_result" >&2
        exit 1
    fi
    echo "$label refresh result: $raw_result"
}

refresh_upgrade_signal_consumers() {
    echo "Refreshing L2 services that apply the upgrade signal schedule..."
    refresh_upgrade_signal "builder execution node" "$L2_BUILDER_RPC"
    refresh_upgrade_signal "builder consensus node" "$L2_BUILDER_OP_RPC"
    refresh_upgrade_signal "client execution node" "$L2_RPC"
    refresh_upgrade_signal "client consensus node" "$L2_CLIENT_OP_RPC"
}

require_cmd cast
require_cmd docker
require_cmd forge
require_cmd jq

if ! [[ "$ACTIVATION_OFFSET" =~ ^[0-9]+$ ]]; then
    echo "UPGRADE_SIGNAL_ACTIVATION_OFFSET must be a non-negative integer" >&2
    exit 1
fi
if ! [[ "$RESCHEDULE_BEFORE_SECONDS" =~ ^[0-9]+$ ]]; then
    echo "UPGRADE_SIGNAL_TEST_RESCHEDULE_BEFORE_SECONDS must be a non-negative integer" >&2
    exit 1
fi
if ! [[ "$RESCHEDULE_DELAY_SECONDS" =~ ^[0-9]+$ ]] || ((RESCHEDULE_DELAY_SECONDS == 0)); then
    echo "UPGRADE_SIGNAL_TEST_RESCHEDULE_DELAY_SECONDS must be a positive integer" >&2
    exit 1
fi
if ((ACTIVATION_OFFSET <= RESCHEDULE_BEFORE_SECONDS)); then
    echo "UPGRADE_SIGNAL_ACTIVATION_OFFSET must be greater than UPGRADE_SIGNAL_TEST_RESCHEDULE_BEFORE_SECONDS" >&2
    exit 1
fi
if ((RESCHEDULE_DELAY_SECONDS <= RESCHEDULE_BEFORE_SECONDS)); then
    echo "UPGRADE_SIGNAL_TEST_RESCHEDULE_DELAY_SECONDS must be greater than UPGRADE_SIGNAL_TEST_RESCHEDULE_BEFORE_SECONDS" >&2
    exit 1
fi

reset_devnet

echo "Starting L1 services..."
compose_base up -d --no-build setup-l1 l1-el l1-cl l1-vc
wait_for_l1_rpc

echo "Generating L2 configs with static hardforks disabled..."
compose_base up --no-build setup-l2

if [[ ! -f "$ROLLUP_JSON" || ! -f "$GENESIS_JSON" ]]; then
    echo "expected L2 configs were not generated" >&2
    exit 1
fi
if jq -e '
    .regolith_time? //
    .canyon_time? //
    .delta_time? //
    .ecotone_time? //
    .fjord_time? //
    .granite_time? //
    .holocene_time? //
    .pectra_blob_schedule_time? //
    .isthmus_time? //
    .jovian_time? //
    .base? //
    empty
' "$ROLLUP_JSON" >/dev/null; then
    echo "rollup config still contains static hardfork config" >&2
    exit 1
fi
if jq -e '
    .config.regolithTime? //
    .config.canyonTime? //
    .config.ecotoneTime? //
    .config.fjordTime? //
    .config.graniteTime? //
    .config.holoceneTime? //
    .config.isthmusTime? //
    .config.jovianTime? //
    .config.osakaTime? //
    .config.base? //
    empty
' "$GENESIS_JSON" >/dev/null; then
    echo "genesis config still contains static hardfork config" >&2
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
UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP="$ACTIVATION_TIMESTAMP" \
    "$SCRIPT_DIR/test-base-azul.sh" before "$L2_RPC" latest

RESCHEDULE_AT_TIMESTAMP="$((ACTIVATION_TIMESTAMP - RESCHEDULE_BEFORE_SECONDS))"
wait_for_l2_timestamp "$RESCHEDULE_AT_TIMESTAMP" "Azul reschedule window"

CURRENT_TIMESTAMP="$(latest_l2_timestamp)"
if ((CURRENT_TIMESTAMP >= ACTIVATION_TIMESTAMP)); then
    echo "missed Azul reschedule window: latest L2 timestamp $CURRENT_TIMESTAMP >= $ACTIVATION_TIMESTAMP" >&2
    exit 1
fi

UPDATED_ACTIVATION_TIMESTAMP="$((CURRENT_TIMESTAMP + RESCHEDULE_DELAY_SECONDS))"
if ((UPDATED_ACTIVATION_TIMESTAMP <= ACTIVATION_TIMESTAMP)); then
    echo "updated Azul timestamp $UPDATED_ACTIVATION_TIMESTAMP must be after original timestamp $ACTIVATION_TIMESTAMP" >&2
    exit 1
fi

echo "Rescheduling Azul from $ACTIVATION_TIMESTAMP to $UPDATED_ACTIVATION_TIMESTAMP..."
set_azul_activation_timestamp "$UPDATED_ACTIVATION_TIMESTAMP"
refresh_upgrade_signal_consumers

CURRENT_TIMESTAMP="$(latest_l2_timestamp)"
if ((CURRENT_TIMESTAMP >= UPDATED_ACTIVATION_TIMESTAMP)); then
    echo "missed delayed pre-activation window: latest L2 timestamp $CURRENT_TIMESTAMP >= $UPDATED_ACTIVATION_TIMESTAMP" >&2
    exit 1
fi

wait_for_l2_timestamp "$ACTIVATION_TIMESTAMP" "original Azul timestamp"

CURRENT_TIMESTAMP="$(latest_l2_timestamp)"
if ((CURRENT_TIMESTAMP >= UPDATED_ACTIVATION_TIMESTAMP)); then
    echo "missed delayed pre-activation window after original timestamp: latest L2 timestamp $CURRENT_TIMESTAMP >= $UPDATED_ACTIVATION_TIMESTAMP" >&2
    exit 1
fi

echo "Running delayed pre-Azul checks at L2 timestamp $CURRENT_TIMESTAMP..."
UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP="$UPDATED_ACTIVATION_TIMESTAMP" \
    "$SCRIPT_DIR/test-base-azul.sh" before "$L2_RPC" latest

wait_for_activation_timestamp "$UPDATED_ACTIVATION_TIMESTAMP"

echo "Running post-Azul checks..."
UPGRADE_SIGNAL_ACTIVATION_TIMESTAMP="$UPDATED_ACTIVATION_TIMESTAMP" \
    "$SCRIPT_DIR/test-base-azul.sh" after "$L2_RPC" latest

cat <<EOF

Contract-driven upgrade signal devnet test passed.

L2 genesis timestamp:          $L2_GENESIS_TIME
Activation offset:             $ACTIVATION_OFFSET seconds
Initial contract Azul time:    $ACTIVATION_TIMESTAMP
Reschedule before activation:  $RESCHEDULE_BEFORE_SECONDS seconds
Reschedule delay:              $RESCHEDULE_DELAY_SECONDS seconds
Updated contract Azul time:    $UPDATED_ACTIVATION_TIMESTAMP
EOF
