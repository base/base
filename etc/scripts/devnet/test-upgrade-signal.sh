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
TX_SPAMMER_ENABLED="${UPGRADE_SIGNAL_TX_SPAMMER:-1}"
TX_SPAMMER_CONFIG="$REPO_ROOT/.devnet/upgrade-signal-load-test.yaml"
TX_SPAMMER_LOG="$REPO_ROOT/.devnet/upgrade-signal-load-test.log"
TX_SPAMMER_OUTPUT="$REPO_ROOT/.devnet/upgrade-signal-load-test-results.json"
TX_SPAMMER_POST_UPGRADE_TXS="$REPO_ROOT/.devnet/upgrade-signal-post-upgrade-txs.json"
TX_SPAMMER_BIN="${UPGRADE_SIGNAL_TX_SPAMMER_BIN:-$REPO_ROOT/target/debug/base-load-tester}"
TX_SPAMMER_POST_TEST_SECONDS="${UPGRADE_SIGNAL_TX_SPAMMER_POST_TEST_SECONDS:-60}"
L1_RPC="${UPGRADE_SIGNAL_L1_RPC_URL:-${L1_RPC_URL:-http://localhost:4545}}"
L2_RPC="${UPGRADE_SIGNAL_L2_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
L2_BUILDER_RPC="${UPGRADE_SIGNAL_L2_BUILDER_RPC_URL:-${L2_BUILDER_RPC_URL:-http://localhost:7545}}"
L2_BUILDER_OP_RPC="${UPGRADE_SIGNAL_L2_BUILDER_OP_RPC_URL:-${L2_BUILDER_OP_RPC_URL:-http://localhost:7549}}"
L2_CLIENT_OP_RPC="${UPGRADE_SIGNAL_L2_CLIENT_OP_RPC_URL:-${L2_CLIENT_OP_RPC_URL:-http://localhost:8549}}"
L2_BUILDER_FLASHBLOCKS_WS="${UPGRADE_SIGNAL_L2_BUILDER_FLASHBLOCKS_WS:-ws://localhost:${L2_BUILDER_FLASHBLOCKS_PORT:-7111}}"
ROLLUP_JSON="$REPO_ROOT/.devnet/l2/configs/rollup.json"
GENESIS_JSON="$REPO_ROOT/.devnet/l2/configs/genesis.json"
TX_SPAMMER_FUNDER_KEY="${UPGRADE_SIGNAL_TX_SPAMMER_FUNDER_KEY:-${ANVIL_ACCOUNT_2_KEY:-}}"
TX_SPAMMER_PID=""

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

latest_l2_block_number() {
    cast block-number --rpc-url "$L2_RPC"
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

tx_spammer_enabled() {
    case "$TX_SPAMMER_ENABLED" in
        1|true|TRUE|yes|YES|on|ON)
            return 0
            ;;
        0|false|FALSE|no|NO|off|OFF)
            return 1
            ;;
        *)
            echo "UPGRADE_SIGNAL_TX_SPAMMER must be a boolean-like value" >&2
            exit 1
            ;;
    esac
}

write_tx_spammer_config() {
    cat > "$TX_SPAMMER_CONFIG" <<EOF
transaction_submission_rpcs:
  - "$L2_BUILDER_RPC"
query_rpc: "$L2_RPC"
txpool_nodes:
  - "$L2_BUILDER_RPC"
  - "$L2_RPC"
flashblocks_ws: "$L2_BUILDER_FLASHBLOCKS_WS"

sender_count: 100
target_gps: 20000000
in_flight_per_sender: 96
batch_size: 20
batch_timeout: "10ms"
duration: "30s"
seed: 12345
funding_amount: "10000000000000000"

transactions:
  - weight: 70
    type: transfer
  - weight: 20
    type: calldata
    max_size: 256
  - weight: 10
    type: precompile
    target: sha256
EOF
}

prepare_tx_spammer() {
    if ! tx_spammer_enabled; then
        return
    fi

    require_cmd cargo
    if [[ -z "$TX_SPAMMER_FUNDER_KEY" ]]; then
        echo "missing tx spammer funder key; set UPGRADE_SIGNAL_TX_SPAMMER_FUNDER_KEY" >&2
        exit 1
    fi

    echo "Building tx spammer..."
    (cd "$REPO_ROOT" && cargo build -p base-load-tester-bin --bin base-load-tester)
    if [[ ! -x "$TX_SPAMMER_BIN" ]]; then
        echo "tx spammer binary not found at $TX_SPAMMER_BIN" >&2
        echo "set UPGRADE_SIGNAL_TX_SPAMMER_BIN when using a custom Cargo target dir" >&2
        exit 1
    fi
}

start_tx_spammer() {
    if ! tx_spammer_enabled; then
        echo "Tx spammer disabled."
        return
    fi

    write_tx_spammer_config
    : > "$TX_SPAMMER_LOG"

    echo "Starting tx spammer against builder RPC $L2_BUILDER_RPC..."
    (
        cd "$REPO_ROOT"
        export FUNDER_KEY="$TX_SPAMMER_FUNDER_KEY"
        export LOAD_TEST_OUTPUT="$TX_SPAMMER_OUTPUT"
        exec "$TX_SPAMMER_BIN" \
            "$TX_SPAMMER_CONFIG" \
            --continuous
    ) >> "$TX_SPAMMER_LOG" 2>&1 &
    TX_SPAMMER_PID="$!"

    sleep 5
    if ! kill -0 "$TX_SPAMMER_PID" >/dev/null 2>&1; then
        echo "tx spammer exited during startup; last log lines:" >&2
        tail -n 80 "$TX_SPAMMER_LOG" >&2 || true
        exit 1
    fi

    echo "Tx spammer started with pid $TX_SPAMMER_PID; logs: $TX_SPAMMER_LOG"
}

stop_tx_spammer() {
    if [[ -z "$TX_SPAMMER_PID" ]]; then
        return
    fi
    if ! kill -0 "$TX_SPAMMER_PID" >/dev/null 2>&1; then
        wait "$TX_SPAMMER_PID" || true
        TX_SPAMMER_PID=""
        return
    fi

    echo "Stopping tx spammer..."
    kill -INT "$TX_SPAMMER_PID" >/dev/null 2>&1 || true
    wait "$TX_SPAMMER_PID" || true
    TX_SPAMMER_PID=""
}

write_post_upgrade_tx_report() {
    local start_block="$1"
    local end_block="$2"
    local tx_blocks="$REPO_ROOT/.devnet/upgrade-signal-post-upgrade-tx-blocks.jsonl"
    local block_number
    local block_hex
    local block_json

    : > "$tx_blocks"

    if ((start_block <= end_block)); then
        for ((block_number = start_block; block_number <= end_block; block_number++)); do
            block_hex="$(printf '0x%x' "$block_number")"
            block_json="$(cast rpc --rpc-url "$L2_RPC" eth_getBlockByNumber "$block_hex" true)"
            jq -c \
                --argjson block_number "$block_number" \
                '{
                    block_number: $block_number,
                    hash: .hash,
                    timestamp: .timestamp,
                    transaction_count: (.transactions | length),
                    transactions: [
                        .transactions[] | {
                            hash,
                            from,
                            to,
                            type,
                            gas,
                            gasPrice,
                            maxFeePerGas,
                            maxPriorityFeePerGas
                        }
                    ]
                }' <<< "$block_json" >> "$tx_blocks"
        done
    fi

    jq -s \
        --argjson start_block "$start_block" \
        --argjson end_block "$end_block" \
        '{
            start_block: $start_block,
            end_block: $end_block,
            total_transactions: (map(.transaction_count) | add // 0),
            blocks: .
        }' "$tx_blocks" > "$TX_SPAMMER_POST_UPGRADE_TXS"
}

keep_tx_spammer_after_test() {
    if ! tx_spammer_enabled || ((TX_SPAMMER_POST_TEST_SECONDS == 0)); then
        return
    fi
    local start_block
    local end_block
    local tx_count

    if [[ -z "$TX_SPAMMER_PID" ]]; then
        return
    fi
    if ! kill -0 "$TX_SPAMMER_PID" >/dev/null 2>&1; then
        echo "tx spammer exited before post-test soak completed; last log lines:" >&2
        tail -n 80 "$TX_SPAMMER_LOG" >&2 || true
        exit 1
    fi

    start_block="$(latest_l2_block_number)"
    echo "Keeping tx spammer running for $TX_SPAMMER_POST_TEST_SECONDS seconds after test completion..."
    sleep "$TX_SPAMMER_POST_TEST_SECONDS"

    if ! kill -0 "$TX_SPAMMER_PID" >/dev/null 2>&1; then
        echo "tx spammer exited during post-test soak; last log lines:" >&2
        tail -n 80 "$TX_SPAMMER_LOG" >&2 || true
        exit 1
    fi

    end_block="$(latest_l2_block_number)"
    write_post_upgrade_tx_report "$((start_block + 1))" "$end_block"
    tx_count="$(jq -r '.total_transactions' "$TX_SPAMMER_POST_UPGRADE_TXS")"
    if ((tx_count == 0)); then
        echo "no post-upgrade tx spam landed between L2 blocks $((start_block + 1)) and $end_block" >&2
        echo "tx spammer logs: $TX_SPAMMER_LOG" >&2
        exit 1
    fi

    echo "Observed $tx_count post-upgrade txs; report: $TX_SPAMMER_POST_UPGRADE_TXS"
}

trap stop_tx_spammer EXIT

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
if ! [[ "$TX_SPAMMER_POST_TEST_SECONDS" =~ ^[0-9]+$ ]]; then
    echo "UPGRADE_SIGNAL_TX_SPAMMER_POST_TEST_SECONDS must be a non-negative integer" >&2
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

prepare_tx_spammer

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
start_tx_spammer

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

keep_tx_spammer_after_test

cat <<EOF

Contract-driven upgrade signal devnet test passed.

L2 genesis timestamp:          $L2_GENESIS_TIME
Activation offset:             $ACTIVATION_OFFSET seconds
Initial contract Azul time:    $ACTIVATION_TIMESTAMP
Reschedule before activation:  $RESCHEDULE_BEFORE_SECONDS seconds
Reschedule delay:              $RESCHEDULE_DELAY_SECONDS seconds
Updated contract Azul time:    $UPDATED_ACTIVATION_TIMESTAMP
Tx spammer post-test soak:     $TX_SPAMMER_POST_TEST_SECONDS seconds
EOF
