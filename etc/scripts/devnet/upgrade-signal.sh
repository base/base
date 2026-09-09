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

ENV_OUT="${UPGRADE_SIGNAL_ENV_OUT:-$REPO_ROOT/.devnet/l2/configs/upgrade-signal.env}"
ADDRESSES_JSON="${UPGRADE_SIGNAL_ADDRESSES_JSON:-$REPO_ROOT/.devnet/l2/configs/l1-addresses.json}"
L1_RPC="${UPGRADE_SIGNAL_L1_RPC_URL:-${L1_RPC_URL:-http://localhost:4545}}"
L2_RPC="${UPGRADE_SIGNAL_L2_RPC_URL:-${L2_CLIENT_RPC_URL:-http://localhost:8545}}"
CONTAINER_L1_RPC="${UPGRADE_SIGNAL_CONTAINER_L1_RPC:-http://l1-el:${L1_HTTP_PORT:-4545}}"
MODE="${UPGRADE_SIGNAL_MODE:-runtime-admin}"
L1_BLOCK_TAG="${UPGRADE_SIGNAL_L1_BLOCK_TAG:-latest}"
MIN_PROTOCOL_VERSION="${UPGRADE_SIGNAL_MIN_PROTOCOL_VERSION:-4294967296}"
FUTURE_OFFSET="${UPGRADE_SIGNAL_ACTIVATION_OFFSET:-3660}"
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
  setup        Uses the real ProtocolVersions deployed by base genesis, writes
               upgrade-signal.env, and applies explicit overrides only.
  set          Updates one or more upgrade activation timestamps on the contract.
               Normal owner, ordering, one-hour notice/freeze rules apply.
               Use timestamp 0 to clear a mutable trailing upgrade.
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
  local env_dir
  env_dir="$(dirname "$ENV_OUT")"
  mkdir -p "$env_dir"

  local writer=(tee "$ENV_OUT")
  if [[ -e "$ENV_OUT" && ! -w "$ENV_OUT" ]] || [[ ! -e "$ENV_OUT" && ! -w "$env_dir" ]]; then
    # setup-devnet creates root-owned bind-mounted configs on Linux. Write only
    # this file through Docker, without elevating the RPC/deployment commands or
    # changing ownership of the configs shared with the running containers.
    require_cmd docker
    env_dir="$(cd "$env_dir" && pwd)"
    writer=(docker run --rm -i --network none -v "$env_dir:/configs" alpine tee "/configs/$(basename "$ENV_OUT")")
  fi

  "${writer[@]}" >/dev/null <<EOF
BASE_NODE_UPGRADE_SIGNAL_CONTRACT=$contract
BASE_NODE_UPGRADE_SIGNAL_L1_RPC=$CONTAINER_L1_RPC
BASE_NODE_UPGRADE_SIGNAL_MODE=$MODE
BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG=$L1_BLOCK_TAG
EOF
}

ensure_contract() {
  wait_l1_rpc
  if [[ -z "$CONTRACT_ADDRESS" ]]; then
    CONTRACT_ADDRESS="$(contract_from_env_file)"
  fi
  if [[ -z "$CONTRACT_ADDRESS" && -f "$ADDRESSES_JSON" ]]; then
    CONTRACT_ADDRESS="$(jq -r '.ProtocolVersionsProxy // empty' "$ADDRESSES_JSON")"
  fi
  local code
  if [[ -z "$CONTRACT_ADDRESS" ]]; then
    echo "ProtocolVersions is missing; generate a fresh devnet with just devnet up" >&2
    exit 1
  fi
  if ! code="$(contract_code "$CONTRACT_ADDRESS")"; then
    echo "failed to read ProtocolVersions bytecode at $CONTRACT_ADDRESS from $L1_RPC" >&2
    exit 1
  fi
  if [[ -z "$code" || "$code" == "0x" ]]; then
    echo "ProtocolVersions has no code at $CONTRACT_ADDRESS; generate a fresh devnet with just devnet up" >&2
    exit 1
  fi
  write_env_file "$CONTRACT_ADDRESS"
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
  if [[ "${#SET_OVERRIDES[@]}" -eq 0 ]]; then
    return
  fi
  require_deployer_key
  local override upgrade timestamp index
  # Execute explicit updates in caller order, so the real registry enforces ordering.
  # Never rewrite imported historical activations or bypass its notice/freeze guards.
  for override in "${SET_OVERRIDES[@]}"; do
    [[ "$override" == *=* ]] || { echo "expected upgrade=timestamp: $override" >&2; exit 1; }
    upgrade="${override%%=*}"
    timestamp="${override#*=}"
    validate_uint "timestamp for $upgrade" "$timestamp"
    index="$(upgrade_index "$upgrade")"
    cast send --rpc-url "$L1_RPC" --private-key "$DEPLOYER_KEY" \
      "$CONTRACT_ADDRESS" "setTimestamp(uint256,uint64)" "$index" "$timestamp" --json >/dev/null
  done
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
    l1_timestamp="$(cast block latest --rpc-url "$L1_RPC" --json | jq -r '.timestamp')"
    if (( l1_timestamp > latest_timestamp )); then latest_timestamp=$((l1_timestamp)); fi
    SET_OVERRIDES+=("${POSITIONAL[0]}=$((latest_timestamp + FUTURE_OFFSET))")
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
