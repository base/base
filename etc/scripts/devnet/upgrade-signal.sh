#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

DEVNET_ENV="$REPO_ROOT/etc/docker/devnet-env"
if [[ -f "$DEVNET_ENV" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$DEVNET_ENV"
  set +a
fi
if [[ -n "${UPGRADE_SIGNAL_ENV_FILES:-}" ]]; then
  IFS=: read -r -a upgrade_signal_env_files <<<"$UPGRADE_SIGNAL_ENV_FILES"
  for env_file in "${upgrade_signal_env_files[@]}"; do
    set -a
    # shellcheck disable=SC1090
    source "$env_file"
    set +a
  done
fi

CONTRACT_ROOT="${CONTRACT_ROOT:-$REPO_ROOT/crates/utilities/test-utils/contracts}"
ENV_OUT="${UPGRADE_SIGNAL_ENV_OUT:-$REPO_ROOT/.devnet/l2/configs/upgrade-signal.env}"
ROLLUP_JSON="${UPGRADE_SIGNAL_ROLLUP_JSON:-$REPO_ROOT/.devnet/l2/configs/rollup.json}"
L1_RPC="${UPGRADE_SIGNAL_L1_RPC_URL:-${L1_RPC_URL:-http://localhost:4545}}"
L2_RPC="${UPGRADE_SIGNAL_L2_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
CONTAINER_L1_RPC="${UPGRADE_SIGNAL_CONTAINER_L1_RPC:-http://l1-el:${L1_HTTP_PORT:-4545}}"
MODE="${UPGRADE_SIGNAL_MODE:-runtime-admin}"
L1_BLOCK_TAG="${UPGRADE_SIGNAL_L1_BLOCK_TAG:-latest}"
MIN_PROTOCOL_VERSION="${UPGRADE_SIGNAL_MIN_PROTOCOL_VERSION:-4294967296}"
FUTURE_OFFSET="${UPGRADE_SIGNAL_ACTIVATION_OFFSET:-120}"
CONTRACT_ADDRESS="${UPGRADE_SIGNAL_CONTRACT:-${BASE_NODE_UPGRADE_SIGNAL_CONTRACT:-}}"

UPGRADE_IDS=()
SCHEDULE=()
SET_OVERRIDES=()
POSITIONAL=()

usage() {
  cat <<'EOF'
Usage:
  upgrade-signal.sh setup [--set upgrade=timestamp ...]
  upgrade-signal.sh set --set upgrade=timestamp [--set upgrade=timestamp ...]
  upgrade-signal.sh set <upgrade> <timestamp>
  upgrade-signal.sh move-future <upgrade> [--offset seconds]
  upgrade-signal.sh status

Commands:
  setup        Deploys MockProtocolVersions if needed, builds the schedule from
               .devnet/l2/configs/rollup.json, applies --set overrides, writes
               upgrade-signal.env, and updates the L1 contract.
  set          Updates one or more upgrade activation timestamps on the contract.
               Use timestamp 0 to clear an upgrade.
  move-future  Sets one upgrade to latest L2 timestamp + --offset seconds.
  status       Prints the configured contract, schedule, and minimum protocol version.
EOF
}

require_cmd() {
  local name="$1"
  if ! command -v "$name" >/dev/null 2>&1; then
    echo "missing required command: $name" >&2
    exit 1
  fi
}

load_upgrade_ids() {
  require_cmd cargo

  local upgrade_ids_csv
  upgrade_ids_csv="$(
    cd "$REPO_ROOT" && cargo run --quiet -p base-upgrade-signal --bin contract_upgrade_ids
  )"

  IFS=, read -r -a UPGRADE_IDS <<<"$upgrade_ids_csv"
  if [[ "${#UPGRADE_IDS[@]}" -eq 0 ]]; then
    echo "failed to load contract upgrade ids" >&2
    exit 1
  fi
}

normalize_upgrade_id() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

upgrade_index() {
  local target
  target="$(normalize_upgrade_id "$1")"

  local i
  for i in "${!UPGRADE_IDS[@]}"; do
    if [[ "${UPGRADE_IDS[$i]}" == "$target" ]]; then
      echo "$i"
      return
    fi
  done

  echo "unknown upgrade id: $1" >&2
  exit 1
}

validate_uint() {
  local name="$1"
  local value="$2"
  if ! [[ "$value" =~ ^[0-9]+$ ]]; then
    echo "$name must be a non-negative integer, got: $value" >&2
    exit 1
  fi
}

require_deployer_key() {
  if [[ -z "${DEPLOYER_KEY:-}" ]]; then
    echo "DEPLOYER_KEY must be set in the environment or etc/docker/devnet-env" >&2
    exit 1
  fi
}

wait_l1_rpc() {
  local retries=120
  local count=0
  until cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1; do
    count=$((count + 1))
    if [[ "$count" -ge "$retries" ]]; then
      echo "L1 RPC not ready at $L1_RPC after $retries retries" >&2
      exit 1
    fi
    sleep 0.5
  done
}

contract_from_env_file() {
  if [[ ! -f "$ENV_OUT" ]]; then
    return 0
  fi

  awk -F= '
    $1 == "BASE_NODE_UPGRADE_SIGNAL_CONTRACT" {
      print $2
      exit
    }
  ' "$ENV_OUT"
}

contract_code() {
  local contract="$1"
  cast code --rpc-url "$L1_RPC" "$contract" 2>/dev/null | tr -d '\r\n'
}

write_env_file() {
  local contract="$1"
  mkdir -p "$(dirname "$ENV_OUT")"

  cat >"$ENV_OUT" <<EOF
BASE_NODE_UPGRADE_SIGNAL_CONTRACT=$contract
BASE_NODE_UPGRADE_SIGNAL_L1_RPC=$CONTAINER_L1_RPC
BASE_NODE_UPGRADE_SIGNAL_MODE=$MODE
BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG=$L1_BLOCK_TAG
EOF
}

deploy_contract() {
  require_cmd forge
  require_deployer_key

  echo "Installing Foundry contract dependencies..."
  (cd "$CONTRACT_ROOT" && forge soldeer install)

  echo "Deploying MockProtocolVersions to $L1_RPC..."
  local deploy_json
  deploy_json="$(
    forge create \
      --root "$CONTRACT_ROOT" \
      --rpc-url "$L1_RPC" \
      --private-key "$DEPLOYER_KEY" \
      --broadcast \
      src/MockProtocolVersions.sol:MockProtocolVersions \
      --json
  )"

  local contract
  contract="$(
    jq -r '.deployedTo // empty' <<<"$deploy_json"
  )"

  if [[ -z "$contract" ]]; then
    echo "failed to parse deployed contract address" >&2
    echo "$deploy_json" >&2
    exit 1
  fi

  CONTRACT_ADDRESS="$contract"
}

ensure_contract() {
  wait_l1_rpc

  if [[ -z "$CONTRACT_ADDRESS" ]]; then
    CONTRACT_ADDRESS="$(contract_from_env_file)"
  fi

  if [[ -n "$CONTRACT_ADDRESS" ]]; then
    local code
    code="$(contract_code "$CONTRACT_ADDRESS" || true)"
    if [[ -n "$code" && "$code" != "0x" ]]; then
      write_env_file "$CONTRACT_ADDRESS"
      return
    fi

    echo "configured upgrade signal contract has no code: $CONTRACT_ADDRESS" >&2
    echo "deploying a fresh MockProtocolVersions contract" >&2
  fi

  deploy_contract
  write_env_file "$CONTRACT_ADDRESS"
}

load_schedule_from_rollup() {
  if [[ ! -f "$ROLLUP_JSON" ]]; then
    echo "rollup config not found: $ROLLUP_JSON" >&2
    exit 1
  fi

  local upgrade_ids_csv
  upgrade_ids_csv="$(IFS=,; printf '%s' "${UPGRADE_IDS[*]}")"

  SCHEDULE=()
  while IFS= read -r value; do
    SCHEDULE+=("$value")
  done < <(
    # Positional, id-ordered. UPGRADE_IDS comes from BaseUpgrade::CONTRACT_VARIANTS.
    # OP Stack fields are top-level; Base-specific upgrades live under .base[$id].
    jq -r --arg upgrade_ids "$upgrade_ids_csv" '
      . as $rollup
      | $rollup.genesis.l2_time as $genesis
      | def signal($value):
          if $value == null then
            0
          elif ($value | tonumber) == 0 then
            ($genesis | tonumber)
          else
            ($value | tonumber)
          end;
        def rollup_value($id):
          if $id == "regolith" then $rollup.regolith_time
          elif $id == "canyon" then $rollup.canyon_time
          elif $id == "delta" then $rollup.delta_time
          elif $id == "ecotone" then $rollup.ecotone_time
          elif $id == "fjord" then $rollup.fjord_time
          elif $id == "granite" then $rollup.granite_time
          elif $id == "holocene" then $rollup.holocene_time
          elif $id == "pectra_blob_schedule" then $rollup.pectra_blob_schedule_time
          elif $id == "isthmus" then $rollup.isthmus_time
          elif $id == "jovian" then $rollup.jovian_time
          else $rollup.base[$id]
          end;
      $upgrade_ids | split(",")[] as $id | signal(rollup_value($id))
    ' "$ROLLUP_JSON"
  )
}

load_schedule_from_contract() {
  SCHEDULE=()
  while IFS= read -r value; do
    SCHEDULE+=("$value")
  done < <(
    cast call --json --rpc-url "$L1_RPC" "$CONTRACT_ADDRESS" "getSchedule()(uint64[])" |
      jq -r '.[0][]'
  )

  while [[ "${#SCHEDULE[@]}" -lt "${#UPGRADE_IDS[@]}" ]]; do
    SCHEDULE+=("0")
  done
}

apply_set_override() {
  local override="$1"
  if [[ "$override" != *=* ]]; then
    echo "override must be upgrade=timestamp, got: $override" >&2
    exit 1
  fi

  local upgrade="${override%%=*}"
  local timestamp="${override#*=}"
  validate_uint "timestamp for $upgrade" "$timestamp"

  local index
  index="$(upgrade_index "$upgrade")"
  SCHEDULE[index]="$timestamp"
}

apply_set_overrides() {
  # Bash 3.2 (macOS) treats "${SET_OVERRIDES[@]}" as unbound under `set -u` when empty.
  if [[ "${#SET_OVERRIDES[@]}" -eq 0 ]]; then
    return
  fi

  local override
  for override in "${SET_OVERRIDES[@]}"; do
    apply_set_override "$override"
  done
}

schedule_arg() {
  local IFS=,
  printf '[%s]' "${SCHEDULE[*]}"
}

update_minimum_protocol_version() {
  validate_uint "minimum protocol version" "$MIN_PROTOCOL_VERSION"
  require_deployer_key

  echo "Updating minimum protocol version to $MIN_PROTOCOL_VERSION..."
  cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    "$CONTRACT_ADDRESS" \
    "setMinimumProtocolVersion(uint256)" \
    "$MIN_PROTOCOL_VERSION" \
    --json >/dev/null
}

update_contract_schedule() {
  require_deployer_key

  echo "Updating upgrade signal schedule..."
  cast send \
    --rpc-url "$L1_RPC" \
    --private-key "$DEPLOYER_KEY" \
    "$CONTRACT_ADDRESS" \
    "setSchedule(uint64[])" \
    "$(schedule_arg)" \
    --json >/dev/null
}

latest_l2_timestamp() {
  local block_json
  local timestamp_hex
  block_json="$(cast rpc --rpc-url "$L2_RPC" eth_getBlockByNumber latest false)"
  timestamp_hex="$(jq -r '.timestamp' <<<"$block_json")"
  if [[ -z "$timestamp_hex" || "$timestamp_hex" == "null" ]]; then
    echo "failed to read latest L2 timestamp from $L2_RPC" >&2
    exit 1
  fi

  printf '%d\n' "$((16#${timestamp_hex#0x}))"
}

print_status() {
  local minimum_version
  # awk strips cast's large-number annotation, e.g. `4294967296 [4.294e9]`.
  minimum_version="$(
    cast call --rpc-url "$L1_RPC" "$CONTRACT_ADDRESS" "minimumProtocolVersion()(uint256)" |
      awk '{print $1}'
  )"

  echo "contract: $CONTRACT_ADDRESS"
  echo "l1 rpc:   $L1_RPC"
  echo "env file: $ENV_OUT"
  echo "minimum protocol version: $minimum_version"
  echo "schedule:"

  local i
  for ((i = 0; i < ${#UPGRADE_IDS[@]}; i++)); do
    printf '  %-22s %s\n' "${UPGRADE_IDS[$i]}" "${SCHEDULE[$i]:-0}"
  done
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi

COMMAND="${1:-setup}"
if [[ $# -gt 0 ]]; then
  shift
fi

while [[ $# -gt 0 ]]; do
  case "$1" in
    --set)
      if [[ -z "${2:-}" ]]; then
        echo "$1 requires a value" >&2
        exit 1
      fi
      SET_OVERRIDES+=("$2")
      shift 2
      ;;
    --offset)
      if [[ -z "${2:-}" ]]; then
        echo "$1 requires a value" >&2
        exit 1
      fi
      FUTURE_OFFSET="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --*)
      echo "unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
    *)
      POSITIONAL+=("$1")
      shift
      ;;
  esac
done

require_cmd cast
require_cmd jq
load_upgrade_ids

case "$COMMAND" in
  setup)
    ensure_contract
    load_schedule_from_rollup
    apply_set_overrides
    update_minimum_protocol_version
    update_contract_schedule
    load_schedule_from_contract
    print_status
    ;;
  set)
    ensure_contract
    if [[ "${#SET_OVERRIDES[@]}" -eq 0 && "${#POSITIONAL[@]}" -eq 2 ]]; then
      SET_OVERRIDES+=("${POSITIONAL[0]}=${POSITIONAL[1]}")
    fi
    if [[ "${#SET_OVERRIDES[@]}" -eq 0 ]]; then
      echo "set requires --set upgrade=timestamp or <upgrade> <timestamp>" >&2
      exit 1
    fi
    load_schedule_from_contract
    apply_set_overrides
    update_contract_schedule
    load_schedule_from_contract
    print_status
    ;;
  move-future)
    ensure_contract
    validate_uint "future offset" "$FUTURE_OFFSET"
    if [[ "${#POSITIONAL[@]}" -ne 1 ]]; then
      echo "move-future requires exactly one upgrade id" >&2
      exit 1
    fi
    load_schedule_from_contract
    latest_timestamp="$(latest_l2_timestamp)"
    SET_OVERRIDES+=("${POSITIONAL[0]}=$((latest_timestamp + FUTURE_OFFSET))")
    apply_set_overrides
    update_contract_schedule
    load_schedule_from_contract
    print_status
    ;;
  status)
    wait_l1_rpc
    if [[ -z "$CONTRACT_ADDRESS" ]]; then
      CONTRACT_ADDRESS="$(contract_from_env_file)"
    fi
    if [[ -z "$CONTRACT_ADDRESS" ]]; then
      echo "no upgrade signal contract configured; run setup first" >&2
      exit 1
    fi
    load_schedule_from_contract
    print_status
    ;;
  *)
    usage >&2
    exit 1
    ;;
esac
