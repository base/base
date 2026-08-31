#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

set -a
# shellcheck source=../../docker/devnet-env
# shellcheck disable=SC1091
source "$REPO_ROOT/etc/docker/devnet-env"
set +a

# Preserve the existing state path for compatibility with prior local runs.
STATE_DIR="$REPO_ROOT/.devnet/anvil-no-nitro"
ADDRESSES_FILE="$STATE_DIR/addresses.json"
ROLLUP_CONFIG="$STATE_DIR/rollup.json"
RUNTIME_ENV="$STATE_DIR/runtime.env"
L2_CONFIG_DIR="$REPO_ROOT/.devnet/l2/configs"
L2_GENESIS="$L2_CONFIG_DIR/genesis.json"
GENERATED_ROLLUP_CONFIG="$L2_CONFIG_DIR/rollup.json"
UPGRADE_SIGNAL_ENV="$L2_CONFIG_DIR/upgrade-signal.env"

L1_RPC="$L1_RPC_URL"
L1_BEACON_RPC="$L1_BEACON_URL"
L2_NODE_RPC="$L2_BASE_RPC_OP_RPC_URL"
L2_PROOFS_RPC="$L2_PROOFS_RPC_URL"
L2_PROOFS_NODE_RPC="$L2_PROOFS_OP_RPC_URL"
L1_CHAIN_ID_VALUE="$L1_CHAIN_ID"
L2_CHAIN_ID_VALUE="$L2_CHAIN_ID"
L1_SLOT_DURATION="${L1_SLOT_DURATION_OVERRIDE:-12}"
CONTAINER_L1_RPC="${UPGRADE_SIGNAL_CONTAINER_L1_RPC:-http://l1-el:$L1_HTTP_PORT}"
MIN_PROTOCOL_VERSION="${UPGRADE_SIGNAL_MIN_PROTOCOL_VERSION:-4294967296}"

CONTRACTS_REPO="https://github.com/base/contracts.git"
CONTRACTS_DIR="$STATE_DIR/contracts"
GAME_TYPE="621"
TEE_IMAGE_HASH="0x0000000000000000000000000000000000000000000000000000000000000000"
NO_NITRO_CONFIG_HASH="0x846b1fd10a5e22fb7572cc4ac794454d301b382c64ab934091e519486e5200be"
NITRO_PROVERS="2"

OWNER_ADDR="$DEPLOYER_ADDR"
OWNER_KEY="$DEPLOYER_KEY"
TEE_PROPOSER_ADDR="$PROPOSER_ADDR"
TEE_CHALLENGER_ADDR="$CHALLENGER_ADDR"

FORK_NAMES=()

usage() {
  cat <<'EOF'
Usage: anvil-nitro-local.sh <bootstrap|status>

Bootstraps or inspects the local Nitro proof stack whose long-running services
are managed by Docker Compose.

Use `just anvil-nitro-local up` to build and start the complete stack.
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

preflight_l1() {
  local chain_id seconds_per_slot genesis_time latest_block block_number_hex block_timestamp_hex
  local block_number block_timestamp slot
  chain_id=$(cast chain-id --rpc-url "$L1_RPC")
  if [ "$chain_id" != "$L1_CHAIN_ID_VALUE" ]; then
    echo "ERROR: L1 chain ID is $chain_id, expected $L1_CHAIN_ID_VALUE" >&2
    exit 1
  fi

  seconds_per_slot=$(curl -fsS "$L1_BEACON_RPC/eth/v1/config/spec" | jq -r '.data.SECONDS_PER_SLOT')
  if [ "$seconds_per_slot" != "$L1_SLOT_DURATION" ]; then
    echo "ERROR: Base-Anvil reports SECONDS_PER_SLOT=$seconds_per_slot, expected $L1_SLOT_DURATION" >&2
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
    echo "       start the devnet with 'just anvil-nitro-local up'" >&2
    exit 1
  fi

  cast rpc --rpc-url "$L1_RPC" debug_getRawHeader latest | jq -e 'type == "string"' >/dev/null
  cast rpc --rpc-url "$L1_RPC" debug_getRawReceipts latest | jq -e 'type == "array"' >/dev/null
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
    base:local ./base reth genesis-output-root --chain /config/genesis.json)
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
  local l2_block_time l2_genesis_block l2_genesis_time schedule='[]' name timestamp

  l2_block_time=$(jq -er '.block_time' "$ROLLUP_CONFIG")
  l2_genesis_block=$(jq -er '.genesis.l2.number' "$ROLLUP_CONFIG")
  l2_genesis_time=$(jq -er '.genesis.l2_time' "$ROLLUP_CONFIG")
  load_fork_names
  for name in "${FORK_NAMES[@]}"; do
    timestamp=$(rollup_timestamp "$name")
    schedule=$(jq -c --argjson timestamp "$timestamp" '. + [$timestamp]' <<<"$schedule")
  done

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
    --argjson protocolVersionsInitialSchedule "$schedule" \
    --argjson protocolVersionsInitialMinimumVersion "$MIN_PROTOCOL_VERSION" \
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
      | .l2GenesisTimestamp = $l2GenesisTimestamp
      | .protocolVersionsInitialSchedule = $protocolVersionsInitialSchedule
      | .protocolVersionsInitialMinimumVersion = $protocolVersionsInitialMinimumVersion' \
    "$source" >"$destination"
}

deploy_contracts() {
  local anchor_block=0 anchor_root deployment verifier protocol_versions
  local schedule_timestamp block_timestamp_hex block_timestamp deadline
  anchor_root=$(genesis_output_root)
  write_deploy_config "$anchor_block" "$anchor_root"
  mkdir -p "$CONTRACTS_DIR/deployments"

  schedule_timestamp=$(jq -er \
    '[.protocolVersionsInitialSchedule[] | select(. != 0)] | max // 0' \
    "$CONTRACTS_DIR/deploy-config/anvil-no-nitro.json")
  deadline=$((SECONDS + 120))
  echo "Waiting for L1 to reach protocol schedule timestamp $schedule_timestamp ..."
  while true; do
    block_timestamp_hex=$(cast rpc --rpc-url "$L1_RPC" eth_getBlockByNumber latest false |
      jq -er '.timestamp')
    block_timestamp=$((block_timestamp_hex))
    [ "$block_timestamp" -ge "$schedule_timestamp" ] && break
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "ERROR: L1 timestamp did not reach $schedule_timestamp within 2 minutes" >&2
      exit 1
    fi
    sleep 1
  done

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

validate_protocol_versions() {
  local registry expected_schedule actual_schedule actual_minimum
  registry=$(address_value ProtocolVersions)
  expected_schedule=$(jq -c '.protocolVersionsInitialSchedule' \
    "$CONTRACTS_DIR/deploy-config/anvil-no-nitro.json")
  actual_schedule=$(cast call --json "$registry" "getSchedule()(uint64[])" --rpc-url "$L1_RPC" |
    jq -c '.[0] | map(tonumber)')
  if [ "$actual_schedule" != "$expected_schedule" ]; then
    echo "ERROR: ProtocolVersions schedule $actual_schedule, expected $expected_schedule" >&2
    exit 1
  fi

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

write_runtime_env() {
  cat >"$RUNTIME_ENV" <<EOF
TEE_PROVER_REGISTRY_ADDRESS=$(address_value TEEProverRegistry)
BASE_PROPOSER_ANCHOR_STATE_REGISTRY_ADDR=$(address_value AnchorStateRegistry)
BASE_PROPOSER_DISPUTE_GAME_FACTORY_ADDR=$(address_value DisputeGameFactory)
EOF
}

register_signer() {
  local number="$1" registry signer
  registry=$(address_value TEEProverRegistry)
  signer=$(prover_signer_address "$number")
  echo "Registering local Nitro signer $number: $signer"
  owner_send "$registry" "addDevSigner(address,bytes32)" "$signer" "$TEE_IMAGE_HASH" >/dev/null
}

cmd_bootstrap() {
  require_tools cast forge jq git just docker cargo curl
  rm -rf "$STATE_DIR"
  mkdir -p "$STATE_DIR"

  preflight_l1
  load_rollup_config
  prepare_contracts
  deploy_contracts
  validate_protocol_versions
  write_upgrade_signal_env
  local number
  for ((number = 1; number <= NITRO_PROVERS; number++)); do
    register_signer "$number"
  done
  write_runtime_env

  echo "Development proof contracts are anchored at L2 genesis."
  echo "  ProtocolVersions: $(address_value ProtocolVersions)"
  echo "  L2 output root:   $(address_value anchorRoot)"
}

cmd_status() {
  if [ ! -f "$ADDRESSES_FILE" ]; then
    echo "No local Nitro deployment found."
    return
  fi

  local number signer registry
  echo "L2 sync"
  cast rpc optimism_syncStatus --rpc-url "$L2_NODE_RPC" |
    jq '{unsafe_l2: .unsafe_l2.number, safe_l2: .safe_l2.number, finalized_l2: .finalized_l2.number}'

  echo ""
  echo "L2 proofs sync"
  cast rpc optimism_syncStatus --rpc-url "$L2_PROOFS_NODE_RPC" |
    jq '{unsafe_l2: .unsafe_l2.number, safe_l2: .safe_l2.number, finalized_l2: .finalized_l2.number}'
  cast rpc debug_proofsSyncStatus --rpc-url "$L2_PROOFS_RPC" | jq .

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

case "${1:-}" in
  bootstrap) cmd_bootstrap ;;
  status) cmd_status ;;
  *) usage >&2; exit 1 ;;
esac
