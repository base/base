#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

set -a
# shellcheck source=../../docker/devnet-env
# shellcheck disable=SC1091
source "$REPO_ROOT/etc/docker/devnet-env"
set +a

# Keep runtime identifiers stable so the renamed command can stop existing stacks.
STATE_DIR="$REPO_ROOT/.devnet/anvil-no-nitro"
LOG_DIR="$STATE_DIR/logs"
PID_DIR="$STATE_DIR/pids"
ADDRESSES_FILE="$STATE_DIR/addresses.json"
ROLLUP_CONFIG="$STATE_DIR/rollup.json"
L2_CONFIG_DIR="$REPO_ROOT/.devnet/l2/configs"
L2_GENESIS="$L2_CONFIG_DIR/genesis.json"
GENERATED_ROLLUP_CONFIG="$L2_CONFIG_DIR/rollup.json"
UPGRADE_SIGNAL_ENV="$L2_CONFIG_DIR/upgrade-signal.env"

L1_RPC="$L1_RPC_URL"
L1_BEACON_RPC="$L1_BEACON_URL"
L2_ETH_RPC="$L2_BASE_RPC_URL"
L2_NODE_RPC="$L2_BASE_RPC_OP_RPC_URL"
L1_CHAIN_ID_VALUE="$L1_CHAIN_ID"
L2_CHAIN_ID_VALUE="$L2_CHAIN_ID"
CONTAINER_L1_RPC="${UPGRADE_SIGNAL_CONTAINER_L1_RPC:-http://l1-el:$L1_HTTP_PORT}"
MIN_PROTOCOL_VERSION="${UPGRADE_SIGNAL_MIN_PROTOCOL_VERSION:-4294967296}"

CONTRACTS_REPO="https://github.com/base/contracts.git"
CONTRACTS_DIR="$STATE_DIR/contracts"
GAME_TYPE="621"
TEE_IMAGE_HASH="0x0000000000000000000000000000000000000000000000000000000000000000"
NO_NITRO_CONFIG_HASH="0x846b1fd10a5e22fb7572cc4ac794454d301b382c64ab934091e519486e5200be"
NITRO_PROVERS="2"

POSTGRES_CONTAINER="anvil-no-nitro-postgres"
POSTGRES_PORT="15433"
PROVER_SERVICE_RPC_PORT="19000"
PROVER_SERVICE_WORKER_RPC_PORT="19001"
PROPOSER_HEALTH_PORT="18080"
PROVER_SERVICE_RPC="http://localhost:$PROVER_SERVICE_RPC_PORT"
PROVER_SERVICE_WORKER_RPC="http://localhost:$PROVER_SERVICE_WORKER_RPC_PORT"

OWNER_ADDR="$DEPLOYER_ADDR"
OWNER_KEY="$DEPLOYER_KEY"
TEE_PROPOSER_KEY="$PROPOSER_KEY"
TEE_PROPOSER_ADDR="$PROPOSER_ADDR"
TEE_CHALLENGER_ADDR="$CHALLENGER_ADDR"

DAEMON_NAMES=()
FORK_NAMES=()

usage() {
  cat <<'EOF'
Usage: anvil-nitro-local.sh <bootstrap|up|down|status|logs>

Runs the local Nitro proof stack against the Docker devnet whose
execution and Beacon L1 APIs are both served by one Base-Anvil process.

The devnet startup calls bootstrap before starting L2 nodes, then calls up
after the L2 is available. Use `just devnet up-anvil-nitro-local` to run both.
EOF
}

require_tools() {
  local tool
  for tool in "$@"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "ERROR: '$tool' is required" >&2
      exit 1
    fi
  done
}

first_word() {
  awk 'NR == 1 { gsub(/[(),]/, "", $1); print $1 }'
}

call_first() {
  cast call "$@" --rpc-url "$L1_RPC" | first_word
}

owner_send() {
  local receipt
  receipt=$(cast send --private-key "$OWNER_KEY" --rpc-url "$L1_RPC" --json "$@")
  if ! jq -e '.status == "0x1" or .status == "1" or .status == 1' <<<"$receipt" >/dev/null; then
    jq -r '"ERROR: transaction \(.transactionHash) reverted with status \(.status)"' \
      <<<"$receipt" >&2
    return 1
  fi
  jq -r '"tx: \(.transactionHash) block=\(.blockNumber) status=\(.status)"' <<<"$receipt"
}

address_value() {
  local key="$1" value
  value=$(jq -r --arg key "$key" '.[$key] // empty' "$ADDRESSES_FILE")
  if [ -z "$value" ]; then
    echo "ERROR: missing $key in $ADDRESSES_FILE" >&2
    exit 1
  fi
  echo "$value"
}

prover_signer_key() {
  printf '0x%064x' "$1"
}

prover_signer_address() {
  cast wallet address --private-key "$(prover_signer_key "$1")"
}

cargo_native_env() {
  local lz4_prefix cpath="${CPATH:-}" library_path="${LIBRARY_PATH:-}"
  if command -v brew >/dev/null 2>&1; then
    lz4_prefix=$(brew --prefix lz4 2>/dev/null || true)
    if [ -n "$lz4_prefix" ] && [ -f "$lz4_prefix/include/lz4.h" ]; then
      cpath="${cpath:+$cpath:}$lz4_prefix/include"
      library_path="${library_path:+$library_path:}$lz4_prefix/lib"
    fi
  fi
  printf 'CPATH=%s\nLIBRARY_PATH=%s\n' "$cpath" "$library_path"
}

run_with_native_env() {
  local item env_args=()
  while IFS= read -r item; do
    env_args+=("$item")
  done < <(cargo_native_env)
  env "${env_args[@]}" "$@"
}

preflight_l1() {
  local chain_id seconds_per_slot genesis_time latest_block block_number_hex block_timestamp_hex
  local block_number block_timestamp slot
  chain_id=$(cast chain-id --rpc-url "$L1_RPC")
  if [ "$chain_id" != "$L1_CHAIN_ID_VALUE" ]; then
    echo "ERROR: L1 chain ID is $chain_id, expected $L1_CHAIN_ID_VALUE" >&2
    exit 1
  fi

  seconds_per_slot=$(curl -fsS "$L1_BEACON_RPC/eth/v1/config/spec" | jq -r '.data.SECONDS_PER_SLOT')
  if [ "$seconds_per_slot" != "4" ]; then
    echo "ERROR: Base-Anvil reports SECONDS_PER_SLOT=$seconds_per_slot, expected 4" >&2
    exit 1
  fi

  genesis_time=$(curl -fsS "$L1_BEACON_RPC/eth/v1/beacon/genesis" |
    jq -er '.data.genesis_time')
  latest_block=$(cast rpc --rpc-url "$L1_RPC" eth_getBlockByNumber latest false)
  block_number_hex=$(jq -er '.number' <<<"$latest_block")
  block_timestamp_hex=$(jq -er '.timestamp' <<<"$latest_block")
  block_number=$((block_number_hex))
  block_timestamp=$((block_timestamp_hex))
  slot=$(((block_timestamp - genesis_time) / seconds_per_slot))
  if [ "$slot" != "$block_number" ]; then
    echo "ERROR: L1 block $block_number maps to Beacon slot $slot; expected identical values" >&2
    echo "       start the devnet with 'just devnet up-anvil-nitro-local'" >&2
    exit 1
  fi

  cast rpc --rpc-url "$L1_RPC" debug_getRawHeader latest | jq -e 'type == "string"' >/dev/null
  cast rpc --rpc-url "$L1_RPC" debug_getRawReceipts latest | jq -e 'type == "array"' >/dev/null
}

wait_for_l2() {
  local deadline=$((SECONDS + 120))
  until cast rpc optimism_syncStatus --rpc-url "$L2_NODE_RPC" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "ERROR: L2 rollup RPC is unavailable at $L2_NODE_RPC" >&2
      exit 1
    fi
    sleep 1
  done
}

load_rollup_config() {
  if [ ! -f "$GENERATED_ROLLUP_CONFIG" ]; then
    echo "ERROR: generated rollup config not found: $GENERATED_ROLLUP_CONFIG" >&2
    exit 1
  fi
  cp "$GENERATED_ROLLUP_CONFIG" "$ROLLUP_CONFIG"

  local rollup_l1_chain_id
  rollup_l1_chain_id=$(jq -r '.l1_chain_id' "$ROLLUP_CONFIG")
  if [ "$rollup_l1_chain_id" != "$L1_CHAIN_ID_VALUE" ]; then
    echo "ERROR: rollup L1 chain ID is $rollup_l1_chain_id, expected $L1_CHAIN_ID_VALUE" >&2
    exit 1
  fi
}

genesis_output_root() {
  local root
  if [ ! -f "$L2_GENESIS" ]; then
    echo "ERROR: generated L2 genesis not found: $L2_GENESIS" >&2
    exit 1
  fi
  root=$(docker run --rm \
    -v "$L2_CONFIG_DIR:/config:ro" \
    base:local reth genesis-output-root --chain /config/genesis.json)
  if ! [[ "$root" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
    echo "ERROR: invalid L2 genesis output root: $root" >&2
    exit 1
  fi
  echo "$root"
}

prepare_contracts() {
  echo "Fetching the latest base/contracts default branch ..."
  git clone --depth 1 --quiet "$CONTRACTS_REPO" "$CONTRACTS_DIR"

  echo "Installing contract dependencies ..."
  # Foundry 1.7 breaks nested submodules when --no-git removes each dependency's Git metadata.
  sed -i.bak 's/forge install --no-git/forge install/' "$CONTRACTS_DIR/justfile"
  (cd "$CONTRACTS_DIR" && just deps)
  mv "$CONTRACTS_DIR/justfile.bak" "$CONTRACTS_DIR/justfile"
}

write_deploy_config() {
  local anchor_block="$1" anchor_root="$2"
  local source="$CONTRACTS_DIR/deploy-config/local.json"
  local destination="$CONTRACTS_DIR/deploy-config/anvil-no-nitro.json"
  local l2_block_time l2_genesis_block l2_genesis_time

  l2_block_time=$(jq -er '.block_time' "$ROLLUP_CONFIG")
  l2_genesis_block=$(jq -er '.genesis.l2.number' "$ROLLUP_CONFIG")
  l2_genesis_time=$(jq -er '.genesis.l2_time' "$ROLLUP_CONFIG")

  jq \
    --arg owner "$OWNER_ADDR" \
    --arg proposer "$TEE_PROPOSER_ADDR" \
    --arg challenger "$TEE_CHALLENGER_ADDR" \
    --arg anchorRoot "$anchor_root" \
    --arg teeImageHash "$TEE_IMAGE_HASH" \
    --arg configHash "$NO_NITRO_CONFIG_HASH" \
    --argjson l2ChainId "$L2_CHAIN_ID_VALUE" \
    --argjson gameType "$GAME_TYPE" \
    --argjson anchorBlock "$anchor_block" \
    --argjson l2BlockTime "$l2_block_time" \
    --argjson l2GenesisBlockNumber "$l2_genesis_block" \
    --argjson l2GenesisTimestamp "$l2_genesis_time" \
    '.finalSystemOwner = $owner
      | .teeProposer = $proposer
      | .teeChallenger = $challenger
      | .l2ChainId = $l2ChainId
      | .multiproofGameType = $gameType
      | .multiproofConfigHash = $configHash
      | .multiproofGenesisBlockNumber = $anchorBlock
      | .multiproofGenesisOutputRoot = $anchorRoot
      | .teeImageHash = $teeImageHash
      | .l2BlockTime = $l2BlockTime
      | .l2GenesisBlockNumber = $l2GenesisBlockNumber
      | .l2GenesisTimestamp = $l2GenesisTimestamp' \
    "$source" >"$destination"
}

deploy_contracts() {
  local anchor_block=0 anchor_root deployment verifier protocol_versions
  anchor_root=$(genesis_output_root)
  write_deploy_config "$anchor_block" "$anchor_root"
  mkdir -p "$CONTRACTS_DIR/deployments"

  echo "Deploying no-Nitro contracts at L2 anchor block $anchor_block ..."
  (
    cd "$CONTRACTS_DIR"
    DEPLOY_CONFIG_PATH="$CONTRACTS_DIR/deploy-config/anvil-no-nitro.json" \
      forge script scripts/multiproof/DeployDevNoNitro.s.sol \
      --rpc-url "$L1_RPC" \
      --broadcast \
      --private-key "$OWNER_KEY"
  )

  deployment="$CONTRACTS_DIR/deployments/${L1_CHAIN_ID_VALUE}-dev-no-nitro.json"
  test -f "$deployment"
  verifier=$(jq -r '.AggregateVerifier' "$deployment")
  protocol_versions=$(call_first "$verifier" "PROTOCOL_VERSIONS()(address)")
  jq \
    --arg protocolVersions "$protocol_versions" \
    --arg anchorRoot "$anchor_root" \
    --argjson gameType "$GAME_TYPE" \
    --argjson anchorBlock "$anchor_block" \
    '. + {
      ProtocolVersions: $protocolVersions,
      gameType: $gameType,
      anchorBlock: $anchorBlock,
      anchorRoot: $anchorRoot
    }' "$deployment" >"$ADDRESSES_FILE"
}

load_fork_names() {
  local names
  names=$(cd "$REPO_ROOT" && cargo run --quiet -p base-upgrade-signal --bin contract_upgrade_ids)
  IFS=, read -r -a FORK_NAMES <<<"$names"
}

rollup_timestamp() {
  local name="$1" path
  case "$name" in
    azul | beryl | cobalt | denim) path=".base.${name}" ;;
    *) path=".${name}_time" ;;
  esac
  jq -r --arg path "$path" '
    (getpath($path | ltrimstr(".") | split("."))) as $ts
    | if $ts == null then 0
      elif $ts == 0 then .genesis.l2_time
      else $ts end' "$ROLLUP_CONFIG"
}

seed_protocol_versions() {
  load_fork_names
  local registry schedule id timestamp minimum actual_minimum
  registry=$(address_value ProtocolVersions)
  schedule=$(cast call --json "$registry" "getSchedule()(uint64[])" --rpc-url "$L1_RPC" |
    jq -r '.[0] | length')
  if [ "$schedule" != "0" ]; then
    echo "ERROR: newly deployed ProtocolVersions schedule is not empty" >&2
    exit 1
  fi

  echo "Registering ${#FORK_NAMES[@]} protocol upgrades ..."
  for ((id = 0; id < ${#FORK_NAMES[@]}; id++)); do
    timestamp=$(rollup_timestamp "${FORK_NAMES[$id]}")
    minimum=0
    [ "$id" -eq 0 ] && minimum="$MIN_PROTOCOL_VERSION"
    owner_send "$registry" "registerUpgrade(uint64,uint256)" "$timestamp" "$minimum" >/dev/null
    echo "  $id ${FORK_NAMES[$id]}=$timestamp"
  done

  actual_minimum=$(call_first "$registry" "minimumProtocolVersion()(uint256)")
  if [ "$actual_minimum" != "$MIN_PROTOCOL_VERSION" ]; then
    echo "ERROR: minimum protocol version is $actual_minimum, expected $MIN_PROTOCOL_VERSION" >&2
    exit 1
  fi
}

write_upgrade_signal_env() {
  local registry
  registry=$(address_value ProtocolVersions)
  cat >"$UPGRADE_SIGNAL_ENV" <<EOF
BASE_NODE_UPGRADE_SIGNAL_CONTRACT=$registry
BASE_NODE_UPGRADE_SIGNAL_L1_RPC=$CONTAINER_L1_RPC
BASE_NODE_UPGRADE_SIGNAL_MODE=runtime-admin
BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG=latest
EOF
}

fresh_postgres() {
  docker rm -f "$POSTGRES_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$POSTGRES_CONTAINER" \
    -p "$POSTGRES_PORT:5432" \
    -e POSTGRES_DB=prover \
    -e POSTGRES_USER=prover \
    -e POSTGRES_PASSWORD=prover \
    -v "$REPO_ROOT/crates/proof/prover-service/db/migrations:/docker-entrypoint-initdb.d:ro" \
    postgres:17-alpine >/dev/null

  local deadline=$((SECONDS + 60))
  until docker exec "$POSTGRES_CONTAINER" pg_isready -U prover -d prover >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      docker logs "$POSTGRES_CONTAINER" >&2
      exit 1
    fi
    sleep 1
  done
}

wait_for_prover_service() {
  local deadline=$((SECONDS + 30))
  until curl -sf -m 2 -X POST -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"health","params":[]}' \
    "$PROVER_SERVICE_RPC" | jq -e '.jsonrpc' >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "ERROR: prover-service did not start; see $LOG_DIR/prover-service.log" >&2
      exit 1
    fi
    sleep 1
  done
}

register_signer() {
  local number="$1" registry signer
  registry=$(address_value TEEProverRegistry)
  signer=$(prover_signer_address "$number")
  echo "Registering local Nitro signer $number: $signer"
  owner_send "$registry" "addDevSigner(address,bytes32)" "$signer" "$TEE_IMAGE_HASH" >/dev/null
}

run_prover_service() {
  exec env \
    POSTGRES_HOST=localhost \
    POSTGRES_PORT="$POSTGRES_PORT" \
    POSTGRES_DB=prover \
    POSTGRES_USER=prover \
    POSTGRES_PASSWORD=prover \
    POSTGRES_SSLMODE=disable \
    "$REPO_ROOT/target/debug/base-prover-service" \
    --rpc-listen-addr "127.0.0.1:$PROVER_SERVICE_RPC_PORT" \
    --worker-rpc-listen-addr "127.0.0.1:$PROVER_SERVICE_WORKER_RPC_PORT"
}

run_nitro_local() {
  local number="$1"
  exec env BASE_ENCLAVE_SIGNER_KEY="$(prover_signer_key "$number")" \
    "$REPO_ROOT/target/debug/base-prover-nitro-host" local \
    --l1-eth-url "$L1_RPC" \
    --l2-eth-url "$L2_ETH_RPC" \
    --l2-node-url "$L2_NODE_RPC" \
    --l1-beacon-url "$L1_BEACON_RPC" \
    --l2-chain-id "$L2_CHAIN_ID_VALUE" \
    --tee-prover-registry-address "$(address_value TEEProverRegistry)" \
    --prover-service-endpoint "$PROVER_SERVICE_WORKER_RPC"
}

run_proposer() {
  exec "$REPO_ROOT/target/debug/base-proposer" \
    --prover-rpc "$PROVER_SERVICE_RPC" \
    --l1-eth-rpc "$L1_RPC" \
    --l2-eth-rpc "$L2_ETH_RPC" \
    --rollup-rpc "$L2_NODE_RPC" \
    --anchor-state-registry-addr "$(address_value AnchorStateRegistry)" \
    --dispute-game-factory-addr "$(address_value DisputeGameFactory)" \
    --game-type "$GAME_TYPE" \
    --health.port "$PROPOSER_HEALTH_PORT" \
    --private-key "$TEE_PROPOSER_KEY"
}

start_daemon() {
  local name="$1"
  shift
  mkdir -p "$LOG_DIR" "$PID_DIR"
  echo "Starting $name; log: $LOG_DIR/$name.log"
  python3 -c 'import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' \
    "$SCRIPT_DIR/anvil-nitro-local.sh" "$@" >"$LOG_DIR/$name.log" 2>&1 &
  echo $! >"$PID_DIR/$name.pid"
}

daemon_names() {
  DAEMON_NAMES=()
  local path
  for path in "$PID_DIR"/*.pid; do
    [ -f "$path" ] || continue
    DAEMON_NAMES+=("$(basename "$path" .pid)")
  done
}

stop_daemons() {
  daemon_names
  [ "${#DAEMON_NAMES[@]}" -gt 0 ] || return 0
  local name pid
  for name in "${DAEMON_NAMES[@]}"; do
    pid=$(cat "$PID_DIR/$name.pid")
    kill -TERM "-$pid" >/dev/null 2>&1 || kill -TERM "$pid" >/dev/null 2>&1 || true
  done
  sleep 2
  for name in "${DAEMON_NAMES[@]}"; do
    pid=$(cat "$PID_DIR/$name.pid")
    kill -KILL "-$pid" >/dev/null 2>&1 || kill -KILL "$pid" >/dev/null 2>&1 || true
    rm -f "$PID_DIR/$name.pid"
  done
}

wait_for_proposal_target() {
  local anchor target deadline block
  anchor=$(jq -r '.anchorBlock' "$ADDRESSES_FILE")
  target=$((anchor + 100))
  deadline=$((SECONDS + 900))
  echo "Waiting for finalized L2 block $target before starting proposer ..."
  while true; do
    block=$(cast rpc optimism_syncStatus --rpc-url "$L2_NODE_RPC" |
      jq -r '.finalized_l2.number // .finalizedL2.number // 0')
    if [[ "$block" =~ ^[0-9]+$ ]] && [ "$block" -ge "$target" ]; then
      return
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "ERROR: finalized L2 did not reach $target within 15 minutes" >&2
      exit 1
    fi
    sleep 2
  done
}

cmd_bootstrap() {
  require_tools cast forge jq git just docker cargo curl
  stop_daemons
  rm -rf "$STATE_DIR"
  mkdir -p "$STATE_DIR"

  preflight_l1
  load_rollup_config
  prepare_contracts
  deploy_contracts
  seed_protocol_versions
  write_upgrade_signal_env

  echo "Development proof contracts are anchored at L2 genesis."
  echo "  ProtocolVersions: $(address_value ProtocolVersions)"
  echo "  L2 output root:   $(address_value anchorRoot)"
}

cmd_up() {
  require_tools cast jq docker cargo curl python3
  if [ ! -f "$ADDRESSES_FILE" ] || [ ! -f "$ROLLUP_CONFIG" ] || [ ! -f "$UPGRADE_SIGNAL_ENV" ]; then
    echo "ERROR: no bootstrapped local Nitro devnet; run 'just devnet up-anvil-nitro-local'" >&2
    exit 1
  fi

  preflight_l1
  wait_for_l2

  echo "Building local prover binaries ..."
  run_with_native_env cargo build -p base-prover-service-bin --bin base-prover-service
  run_with_native_env cargo build \
    -p base-prover-nitro-host --bin base-prover-nitro-host --features local
  run_with_native_env cargo build -p base-proposer-bin --bin base-proposer

  fresh_postgres
  start_daemon prover-service _prover-service
  wait_for_prover_service

  local number
  for ((number = 1; number <= NITRO_PROVERS; number++)); do
    register_signer "$number"
  done
  for ((number = 1; number <= NITRO_PROVERS; number++)); do
    start_daemon "nitro-prover-$number" _nitro-local "$number"
  done

  wait_for_proposal_target
  start_daemon proposer _proposer

  echo ""
  echo "Single-L1 local Nitro stack is running."
  echo "  L1 execution + Beacon: $L1_RPC / $L1_BEACON_RPC (chain $L1_CHAIN_ID_VALUE)"
  echo "  L2 execution + rollup: $L2_ETH_RPC / $L2_NODE_RPC (chain $L2_CHAIN_ID_VALUE)"
  echo "  TEE registry:           $(address_value TEEProverRegistry)"
  echo "  Dispute game factory:   $(address_value DisputeGameFactory)"
  echo "  Logs:                   $LOG_DIR"
}

cmd_down() {
  stop_daemons
  docker rm -f "$POSTGRES_CONTAINER" >/dev/null 2>&1 || true
  echo "Local Nitro proof stack stopped."
}

cmd_status() {
  echo "Daemons"
  daemon_names
  local name pid state number signer registry
  if [ "${#DAEMON_NAMES[@]}" -eq 0 ]; then
    echo "  none"
  else
    for name in "${DAEMON_NAMES[@]}"; do
      pid=$(cat "$PID_DIR/$name.pid")
      state=dead
      kill -0 "$pid" >/dev/null 2>&1 && state=running
      echo "  $name: $state (pid $pid)"
    done
  fi

  echo ""
  echo "L2 sync"
  cast rpc optimism_syncStatus --rpc-url "$L2_NODE_RPC" |
    jq '{unsafe_l2: .unsafe_l2.number, safe_l2: .safe_l2.number, finalized_l2: .finalized_l2.number}'

  if [ ! -f "$ADDRESSES_FILE" ]; then
    echo "No local Nitro deployment found."
    return
  fi

  echo ""
  echo "Contracts"
  jq -r 'to_entries[] | select(.value | type == "string") | "  \(.key): \(.value)"' \
    "$ADDRESSES_FILE"

  registry=$(address_value TEEProverRegistry)
  echo ""
  echo "Nitro signers"
  for ((number = 1; number <= NITRO_PROVERS; number++)); do
    signer=$(prover_signer_address "$number")
    echo "  $signer registered=$(call_first "$registry" "isRegisteredSigner(address)(bool)" "$signer")"
  done

  local factory count
  factory=$(address_value DisputeGameFactory)
  count=$(call_first "$factory" "gameCount()(uint256)")
  echo ""
  echo "Dispute games: $count"
  if [ "$count" -gt 0 ]; then
    local latest_game
    latest_game=$(cast call "$factory" "gameAtIndex(uint256)(uint32,uint64,address)" \
      "$((count - 1))" --rpc-url "$L1_RPC" | awk 'NR == 3 { print $1 }')
    echo "  latest: $latest_game"
    echo "  target L2 block: $(call_first "$latest_game" "l2SequenceNumber()(uint256)")"
    echo "  accepted proofs: $(call_first "$latest_game" "proofCount()(uint8)")"
    echo "  TEE proposer: $(call_first "$latest_game" "teeProver()(address)")"
  fi
}

cmd_logs() {
  daemon_names
  if [ "${#DAEMON_NAMES[@]}" -eq 0 ]; then
    echo "No proof-stack logs found." >&2
    exit 1
  fi
  local name files=()
  for name in "${DAEMON_NAMES[@]}"; do
    [ -f "$LOG_DIR/$name.log" ] && files+=("$LOG_DIR/$name.log")
  done
  if [ "${#files[@]}" -eq 0 ]; then
    echo "No proof-stack logs found." >&2
    exit 1
  fi
  exec tail -n 50 -F "${files[@]}"
}

case "${1:-}" in
  bootstrap) cmd_bootstrap ;;
  up) cmd_up ;;
  down) cmd_down ;;
  status) cmd_status ;;
  logs) cmd_logs ;;
  _prover-service) run_prover_service ;;
  _nitro-local) run_nitro_local "${2:?prover number required}" ;;
  _proposer) run_proposer ;;
  *) usage >&2; exit 1 ;;
esac
