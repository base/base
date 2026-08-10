#!/usr/bin/env bash
set -euo pipefail

# Local no-Nitro proving stack. By default contracts are deployed to a fresh
# Anvil L1 for component smoke tests. For real proofs, set L1_RPC_URL to the
# live L2's origin L1 or fork it into Anvil with ANVIL_FORK_URL.
#
# `up` runs the five steps of the minimal plan:
#   1. Start Anvil unless L1_RPC_URL is set, then deploy the dev multiproof
#      contracts (DeployDevNoNitro.s.sol from base/contracts main).
#   2. Start base-prover-service with a fresh Postgres database.
#   3. Start two base-prover-nitro-host daemons in local mode (no AWS Nitro).
#   4. Register both local signers on DevTEEProverRegistry.
#   5. Start the proposer.
#
# Real proofs require a live L2 and its L1 execution and beacon endpoints. The
# contracts must be deployed to that same L1 because AggregateVerifier checks
# the proof's L1 origin against its own chain history. Without a live L2 the
# provers idle and the proposer retries in its log.
#
# Normally driven by `just anvil-no-nitro up`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Capture caller overrides before sourcing devnet-env below, which also
# defines these names for the docker devnet (e.g. L1_RPC_URL=...:4545 and
# L2_CHAIN_ID=84538453) and must not silently repoint this stack at it.
CALLER_L1_RPC_URL="${L1_RPC_URL:-}"
CALLER_L1_CHAIN_ID="${L1_CHAIN_ID:-}"
CALLER_L2_CHAIN_ID="${L2_CHAIN_ID:-}"
CALLER_L2_ETH_RPC="${L2_ETH_RPC:-}"
CALLER_L2_NODE_RPC="${L2_NODE_RPC:-}"
CALLER_L1_BEACON_RPC="${L1_BEACON_RPC:-}"
CALLER_ANVIL_FORK_URL="${ANVIL_FORK_URL:-}"

# Anvil's well-known dev accounts (same assignments as etc/docker/devnet-env).
# shellcheck source=etc/docker/devnet-env
source "$REPO_ROOT/etc/docker/devnet-env"

# A sourced assignment remains exported when the caller originally exported
# that name. Restore the caller's values before recursively starting daemons,
# or devnet-env's Docker endpoints would replace non-Docker overrides there.
for caller_name in L1_RPC_URL L1_CHAIN_ID L2_CHAIN_ID L2_ETH_RPC L2_NODE_RPC L1_BEACON_RPC ANVIL_FORK_URL; do
  caller_value_name="CALLER_$caller_name"
  if [ -n "${!caller_value_name}" ]; then
    export "$caller_name=${!caller_value_name}"
  else
    unset "$caller_name"
  fi
done

STATE_DIR="$REPO_ROOT/.devnet/anvil-no-nitro"
LOG_DIR="$STATE_DIR/logs"
PID_DIR="$STATE_DIR/pids"
ADDRESSES_FILE="$STATE_DIR/addresses.json"
# Snapshot of the rollup config served by an L2 node (optimism_rollupConfig),
# present only when L2_NODE_RPC is reachable at `up` time. The provers derive
# their schedule id from the node-served config, so it is the only safe source
# for seeding the ProtocolVersions ladder.
ROLLUP_CONFIG="$STATE_DIR/rollup.json"

ANVIL_PORT="${ANVIL_PORT:-8545}"
L1_RPC="${CALLER_L1_RPC_URL:-http://localhost:$ANVIL_PORT}"
L1_CHAIN_ID_VALUE="${CALLER_L1_CHAIN_ID:-1337}"
# A forked Anvil has the canonical history needed by the verifier contracts,
# but not reth's raw-header/receipt debug RPCs required for witness generation.
L1_PROOF_RPC="$L1_RPC"
[ -z "$CALLER_ANVIL_FORK_URL" ] || L1_PROOF_RPC="$CALLER_ANVIL_FORK_URL"

# Optional live L2. The defaults are deliberately dead endpoints: without a
# live L2 the provers idle and the proposer retries, by design.
L2_ETH_RPC="${CALLER_L2_ETH_RPC:-http://localhost:58645}"
L2_NODE_RPC="${CALLER_L2_NODE_RPC:-http://localhost:58649}"
L1_BEACON_RPC="${CALLER_L1_BEACON_RPC:-http://localhost:54052}"
# Match the local Docker devnet by default; override with the real L2's ID when
# pointing the RPCs above elsewhere. Live runs load the exact node-served config.
L2_CHAIN_ID_VALUE="${CALLER_L2_CHAIN_ID:-84538453}"

POSTGRES_CONTAINER="anvil-no-nitro-postgres"
POSTGRES_PORT_VALUE="${ANVIL_NO_NITRO_POSTGRES_PORT:-15433}"
PROVER_SERVICE_RPC_PORT_VALUE="${PROVER_SERVICE_RPC_PORT:-19000}"
PROVER_SERVICE_WORKER_RPC_PORT_VALUE="${PROVER_SERVICE_WORKER_RPC_PORT:-19001}"
PROPOSER_HEALTH_PORT_VALUE="${PROPOSER_HEALTH_PORT:-18080}"
PROVER_SERVICE_RPC="http://localhost:$PROVER_SERVICE_RPC_PORT_VALUE"
PROVER_SERVICE_WORKER_RPC="http://localhost:$PROVER_SERVICE_WORKER_RPC_PORT_VALUE"

GAME_TYPE="${NO_NITRO_GAME_TYPE:-621}"
NITRO_PROVERS="${NO_NITRO_PROVERS:-2}"

ZERO_HASH="0x0000000000000000000000000000000000000000000000000000000000000000"
TEE_IMAGE_HASH="${TEE_IMAGE_HASH:-$ZERO_HASH}"
NO_NITRO_CONFIG_HASH="${NO_NITRO_CONFIG_HASH:-}"
if [ -z "$NO_NITRO_CONFIG_HASH" ]; then
  case "$L2_CHAIN_ID_VALUE" in
    8453) NO_NITRO_CONFIG_HASH="0x1607709d90d40904f790574404e2ad614eac858f6162faa0ec34c6bf5e5f3c57" ;;
    84532) NO_NITRO_CONFIG_HASH="0x12e9c45f19f9817c6d4385fad29e7a70c355502cf0883e76a9a7e478a85d1360" ;;
    763360) NO_NITRO_CONFIG_HASH="0xd14ddabfc0ad1dd737d6e5917cf271fd479bd539c9b3d85a602589c679a9983a" ;;
    84538453) NO_NITRO_CONFIG_HASH="0x846b1fd10a5e22fb7572cc4ac794454d301b382c64ab934091e519486e5200be" ;;
  esac
fi
# Placeholder anchor output root used when no L2 node is reachable. Any
# nonzero bytes32 works for a from-scratch dev deployment.
DEFAULT_ANCHOR_ROOT="0x0000000000000000000000000000000000000000000000000000000000000001"

CONTRACTS_REPO="${BASE_CONTRACTS_REPO:-https://github.com/base/contracts.git}"
CONTRACTS_REF="${BASE_CONTRACTS_REF:-main}"
CONTRACTS_DIR="${BASE_CONTRACTS_DIR:-$STATE_DIR/contracts}"

OWNER_ADDR="${DEPLOYER_ADDR:?DEPLOYER_ADDR not set}"
OWNER_KEY="${DEPLOYER_KEY:?DEPLOYER_KEY not set}"
TEE_PROPOSER_ADDR="${PROPOSER_ADDR:?PROPOSER_ADDR not set}"
TEE_PROPOSER_KEY="${PROPOSER_KEY:?PROPOSER_KEY not set}"
TEE_CHALLENGER_ADDR="${CHALLENGER_ADDR:?CHALLENGER_ADDR not set}"

# ProtocolVersions registration order, loaded from
# BaseUpgrade::CONTRACT_VARIANTS via the contract_upgrade_ids bin (same
# mechanism as etc/scripts/devnet/upgrade-signal.sh): the prover derives the
# schedule id by position, so the order must come from the one source of truth.
FORK_NAMES=()

load_fork_names() {
  [ "${#FORK_NAMES[@]}" -gt 0 ] && return 0
  local fork_names_csv
  # Explicit status check: sync_protocol_versions runs under `|| true` in the
  # schedule-sync daemon, which suppresses errexit for callees like this one.
  if ! fork_names_csv=$(
    cd "$REPO_ROOT" && cargo run --quiet -p base-upgrade-signal --bin contract_upgrade_ids
  ); then
    echo "WARN: failed to load contract upgrade ids via cargo" >&2
    return 1
  fi
  IFS=, read -r -a FORK_NAMES <<<"$fork_names_csv"
  if [ "${#FORK_NAMES[@]}" -eq 0 ]; then
    echo "WARN: contract_upgrade_ids returned no upgrade ids" >&2
    return 1
  fi
}

DAEMON_NAMES=()

usage() {
  cat <<'EOF'
Usage: anvil-no-nitro.sh <command>

Commands:
  up       Deploy DeployDevNoNitro contracts, start prover-service with a
           fresh database, register two local nitro signers, and run the
           nitro provers (local mode) and proposer as background daemons.
           Starts Anvil unless L1_RPC_URL selects an existing L1.
  down     Stop all daemons and remove the Postgres container.
  sync     Re-fetch the L2's rollup config and mirror its fork timestamps
           onto the deployed ProtocolVersions ladder (register + setTimestamp).
           `up` also runs a schedule-sync daemon doing this periodically.
  status   Print daemon, contract, signer, and latest game status.
  logs     Tail the anvil, prover-service, nitro prover, and proposer logs.

State lives in .devnet/anvil-no-nitro/ and is reset by `up`.

Standalone Anvil mode only smoke-tests component startup. To produce real
proofs and proposals, deploy to or fork from the live L2's origin L1 and
provide all four of its endpoints:
  L1_RPC_URL     Origin L1 execution RPC; contracts deploy here directly
  L2_ETH_RPC     L2 execution RPC          (default: dead endpoint)
  L2_NODE_RPC    L2 rollup node RPC        (default: dead endpoint)
  L1_BEACON_RPC  L1 beacon API             (default: dead endpoint)
  L2_CHAIN_ID    L2 chain id               (default: 84538453, Docker devnet)
When L2_NODE_RPC is reachable at `up` time, the anchor state and the
ProtocolVersions ladder are seeded from it; otherwise placeholder anchors are
deployed, the ladder is left empty, and the provers/proposer idle. Re-run `up`
with the live endpoints to replace a placeholder deployment before proving.

Local Docker devnet example:
  L1_RPC_URL=http://localhost:4545 L1_BEACON_RPC=http://localhost:4052 \
  L2_ETH_RPC=http://localhost:8645 L2_NODE_RPC=http://localhost:8649 \
  L2_CHAIN_ID=84538453 just anvil-no-nitro up

For an isolated snapshot test, omit L1_RPC_URL and set ANVIL_FORK_URL to the
origin L1. Contracts deploy to the fork while proof inputs are read from the
origin RPC, whose raw debug methods Anvil does not implement. The L2 finalized
head must already include one proposal interval beyond NO_NITRO_ANCHOR_BLOCK.
Re-run `up` after the origin L1 advances.

Environment overrides:
  ANVIL_PORT              Anvil listen port (default: 8545)
  ANVIL_FORK_URL          Origin L1 to fork for an isolated proof test
  L1_CHAIN_ID             Contract/origin L1 chain id (default: 1337)
  ANVIL_NO_NITRO_POSTGRES_PORT
                          Prover database port (default: 15433)
  PROVER_SERVICE_RPC_PORT Requester RPC port (default: 19000)
  PROVER_SERVICE_WORKER_RPC_PORT
                          Worker RPC port (default: 19001)
  PROPOSER_HEALTH_PORT    Proposer health port (default: 18080)
  NO_NITRO_PROVERS        Number of local nitro provers (default: 2)
  NO_NITRO_ANCHOR_BLOCK   Anchor L2 block (default: node finalized_l2, else 0)
  NO_NITRO_CONFIG_HASH    AggregateVerifier config hash (derived for built-in chains)
  TEE_IMAGE_HASH          TEE image hash (default: zero for local mode)
  BASE_CONTRACTS_DIR      Existing base/contracts checkout to use
  BASE_CONTRACTS_REF      base/contracts git ref to fetch (default: main)
  SCHEDULE_SYNC_INTERVAL_SECS
                          Schedule-sync daemon poll interval (default: 30)
EOF
}

require_tools() {
  local tool
  for tool in "$@"; do
    command -v "$tool" >/dev/null 2>&1 || {
      echo "ERROR: '$tool' is required" >&2
      exit 1
    }
  done
}

first_word() {
  awk 'NR == 1 { gsub(/[(),]/, "", $1); print $1 }'
}

call_first() {
  local output
  output=$(cast call "$@" --rpc-url "$L1_RPC")
  printf '%s\n' "$output" | first_word
}

owner_send() {
  cast send --private-key "$OWNER_KEY" --rpc-url "$L1_RPC" --json "$@" |
    jq -r '"tx: \(.transactionHash) block=\(.blockNumber) status=\(.status)"'
}

address_value() {
  local key="$1" value
  if [ ! -f "$ADDRESSES_FILE" ]; then
    echo "ERROR: no addresses at $ADDRESSES_FILE; run 'just anvil-no-nitro up' first" >&2
    exit 1
  fi
  value=$(jq -r --arg key "$key" '.[$key] // empty' "$ADDRESSES_FILE")
  if [ -z "$value" ]; then
    echo "ERROR: missing $key in $ADDRESSES_FILE" >&2
    exit 1
  fi
  echo "$value"
}

# Deterministic dev-only signer key for local nitro prover n (1 -> 0x..01, ...).
prover_signer_key() {
  local n="$1"
  printf '0x%064x' "$n"
}

prover_signer_address() {
  cast wallet address --private-key "$(prover_signer_key "$1")"
}

cargo_native_env() {
  local lz4_prefix cpath library_path
  cpath="${CPATH:-}"
  library_path="${LIBRARY_PATH:-}"

  if command -v brew >/dev/null 2>&1; then
    lz4_prefix=$(brew --prefix lz4 2>/dev/null || true)
    if [ -n "$lz4_prefix" ] && [ -f "$lz4_prefix/include/lz4.h" ]; then
      cpath="${cpath:+$cpath:}$lz4_prefix/include"
      library_path="${library_path:+$library_path:}$lz4_prefix/lib"
    fi
  fi

  printf 'CPATH=%s\n' "$cpath"
  printf 'LIBRARY_PATH=%s\n' "$library_path"
}

run_with_native_env() {
  local native_env
  local native_env_args=()
  while IFS= read -r native_env; do
    native_env_args+=("$native_env")
  done < <(cargo_native_env)
  env "${native_env_args[@]}" "$@"
}

wait_for_l1() {
  local deadline=$((SECONDS + 30)) chain_id
  while true; do
    if chain_id=$(cast chain-id --rpc-url "$L1_RPC" 2>/dev/null) &&
      [ "$chain_id" = "$L1_CHAIN_ID_VALUE" ]; then
      return
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "ERROR: L1 RPC not ready after 30s at $L1_RPC" >&2
      exit 1
    fi
    sleep 1
  done
}

l2_node_available() {
  cast rpc optimism_syncStatus --rpc-url "$L2_NODE_RPC" >/dev/null 2>&1
}

prepare_checkout() {
  if [ -n "${BASE_CONTRACTS_DIR:-}" ]; then
    if [ ! -d "$CONTRACTS_DIR/.git" ]; then
      echo "ERROR: contracts checkout not found at $CONTRACTS_DIR" >&2
      exit 1
    fi
    return
  fi

  if [ ! -d "$CONTRACTS_DIR/.git" ]; then
    git init -q "$CONTRACTS_DIR"
    git -C "$CONTRACTS_DIR" remote add origin "$CONTRACTS_REPO"
  fi
  echo "Fetching $CONTRACTS_REPO @ $CONTRACTS_REF ..."
  git -C "$CONTRACTS_DIR" fetch --depth 1 origin "$CONTRACTS_REF"
  git -C "$CONTRACTS_DIR" checkout -q --detach FETCH_HEAD
}

ensure_contract_deps() {
  if [ -f "$CONTRACTS_DIR/lib/forge-std/src/Script.sol" ] &&
    [ -f "$CONTRACTS_DIR/lib/solady/src/utils/Clone.sol" ] &&
    [ -f "$CONTRACTS_DIR/lib/risc0-ethereum/contracts/src/IRiscZeroVerifier.sol" ]; then
    return
  fi

  require_tools just
  echo "Installing pinned contract dependencies in $CONTRACTS_DIR ..."
  # A pinned dependency's own nested submodule setup can fail under `forge install --no-git` on
  # some git/forge versions (e.g. solady's `lib/ds-test` colliding with solmate's identically-named
  # submodule path under `.git/modules`); tolerate that and self-heal any pin still missing below by
  # cloning it directly at its pinned commit, which needs no submodule step for the source files we use.
  just -f "$CONTRACTS_DIR/justfile" -d "$CONTRACTS_DIR" deps ||
    echo "WARN: 'just deps' failed; self-healing any pinned dependency still missing" >&2

  local pin owner_repo sha repo_name
  while IFS= read -r pin; do
    owner_repo="${pin%@*}"
    sha="${pin##*@}"
    repo_name="${owner_repo##*/}"
    [ -d "$CONTRACTS_DIR/lib/$repo_name" ] && continue
    echo "Self-healing missing dependency $repo_name@$sha ..."
    git clone -q "https://$owner_repo" "$CONTRACTS_DIR/lib/$repo_name"
    git -C "$CONTRACTS_DIR/lib/$repo_name" checkout -q "$sha"
    # Some pins (e.g. risc0-ethereum) need their own nested submodules to compile; others (e.g.
    # solady's ds-test, which only backs its own test suite) don't, and initializing them is what
    # broke under `forge install --no-git` above. Best-effort init here, then drop all git metadata.
    git -C "$CONTRACTS_DIR/lib/$repo_name" submodule update --init --recursive >/dev/null 2>&1 || true
    find "$CONTRACTS_DIR/lib/$repo_name" -iname ".git" -exec rm -rf {} +
  done < <(grep -oE 'github\.com/[^/]+/[^@[:space:]]+@[0-9a-f]{40}' "$CONTRACTS_DIR/justfile")

  if [ ! -f "$CONTRACTS_DIR/lib/solady/src/utils/Clone.sol" ] ||
    [ ! -f "$CONTRACTS_DIR/lib/risc0-ethereum/contracts/src/IRiscZeroVerifier.sol" ]; then
    echo "ERROR: contract dependencies are still missing in $CONTRACTS_DIR/lib after 'just deps'" >&2
    exit 1
  fi
}

# Snapshots the rollup config the L2 node serves over RPC; the provers load
# this exact config through the preimage oracle, so all schedule seeding and
# deploy-config genesis constants must come from it.
fetch_rollup_config() {
  if ! cast rpc optimism_rollupConfig --rpc-url "$L2_NODE_RPC" | jq . >"$ROLLUP_CONFIG"; then
    echo "ERROR: optimism_rollupConfig failed at $L2_NODE_RPC" >&2
    exit 1
  fi
  echo "Saved node-served rollup config: $ROLLUP_CONFIG"
}

# Verifies that the contract L1 contains the L2's canonical history. Merely
# matching chain IDs is insufficient for local chains, which commonly use 1337
# with unrelated histories.
validate_l1_for_l2() {
  local l1_chain_id rollup_l1_chain_id sync_status l1_head_hash
  l1_chain_id=$(cast chain-id --rpc-url "$L1_RPC")
  rollup_l1_chain_id=$(jq -r '.l1_chain_id // empty' "$ROLLUP_CONFIG")
  if [ "$l1_chain_id" != "$rollup_l1_chain_id" ]; then
    echo "ERROR: L1 RPC chain id $l1_chain_id does not match the L2 rollup config's" \
      "L1 chain id $rollup_l1_chain_id" >&2
    exit 1
  fi

  sync_status=$(cast rpc optimism_syncStatus --rpc-url "$L2_NODE_RPC")
  l1_head_hash=$(printf '%s\n' "$sync_status" |
    jq -r '.finalized_l1.hash // .finalizedL1.hash // empty')
  if [ -z "$l1_head_hash" ] ||
    ! cast block "$l1_head_hash" --rpc-url "$L1_RPC" >/dev/null 2>&1; then
    echo "ERROR: L1 RPC $L1_RPC does not contain the L2's finalized L1 head" \
      "($l1_head_hash). Deploy contracts to the L2's origin L1; a separate" \
      "Anvil chain cannot verify the proof's embedded L1 origin." >&2
    exit 1
  fi
}

resolve_anchor_block() {
  if [ -n "${NO_NITRO_ANCHOR_BLOCK:-}" ]; then
    echo "$NO_NITRO_ANCHOR_BLOCK"
    return
  fi

  local sync_status block
  sync_status=$(cast rpc optimism_syncStatus --rpc-url "$L2_NODE_RPC")
  block=$(printf '%s\n' "$sync_status" | jq -r '.finalized_l2.number // .finalizedL2.number // empty')
  if [ -z "$block" ] || [ "$block" = "null" ]; then
    echo "ERROR: could not read finalized_l2.number from optimism_syncStatus at $L2_NODE_RPC" >&2
    exit 1
  fi
  echo "$block"
}

output_root_at_block() {
  local block="$1" block_hex output_root response
  printf -v block_hex '0x%x' "$block"
  response=$(cast rpc optimism_outputAtBlock "$block_hex" --rpc-url "$L2_NODE_RPC" 2>/dev/null) || {
    echo "ERROR: optimism_outputAtBlock($block_hex) failed at $L2_NODE_RPC" >&2
    exit 1
  }
  output_root=$(printf '%s\n' "$response" | jq -r '.outputRoot // .output_root // empty')
  if [ -z "$output_root" ] || [ "$output_root" = "null" ]; then
    echo "ERROR: could not read outputRoot from optimism_outputAtBlock($block_hex) at $L2_NODE_RPC" >&2
    exit 1
  fi
  echo "$output_root"
}

# Writes the DeployDevNoNitro deploy config. The AggregateVerifier pins each
# game's schedule to ProtocolVersions.activatedScheduleId(claimTimestamp),
# derived from the L2 genesis constants below; with a live L2 they must match
# the rollup config the provers run with exactly. Without one, placeholders
# make the deployment self-consistent but unprovable (nothing proves anyway).
write_deploy_config() {
  local anchor_block="$1" anchor_root="$2"
  local source_config="$CONTRACTS_DIR/deploy-config/local.json"
  local config_path="$CONTRACTS_DIR/deploy-config/anvil-no-nitro.json"

  if [ -z "$NO_NITRO_CONFIG_HASH" ]; then
    echo "ERROR: no built-in config hash for L2 chain $L2_CHAIN_ID_VALUE;" \
      "set NO_NITRO_CONFIG_HASH explicitly" >&2
    exit 1
  fi
  if [ ! -f "$source_config" ]; then
    echo "ERROR: missing $source_config in contracts checkout" >&2
    exit 1
  fi

  local l2_block_time l2_genesis_block l2_genesis_time value
  if [ -f "$ROLLUP_CONFIG" ]; then
    l2_block_time=$(jq -r '.block_time' "$ROLLUP_CONFIG")
    l2_genesis_block=$(jq -r '.genesis.l2.number' "$ROLLUP_CONFIG")
    l2_genesis_time=$(jq -r '.genesis.l2_time' "$ROLLUP_CONFIG")
    # Fail fast on a missing/renamed field: jq -r turns it into the string
    # "null", which --argjson would happily write into the deploy config as
    # JSON null, only to blow up much later inside forge script.
    for value in "$l2_block_time" "$l2_genesis_block" "$l2_genesis_time"; do
      case "$value" in
        '' | *[!0-9]*)
          echo "ERROR: non-numeric field in $ROLLUP_CONFIG:" \
            "block_time=$l2_block_time genesis.l2.number=$l2_genesis_block" \
            "genesis.l2_time=$l2_genesis_time" >&2
          exit 1
          ;;
      esac
    done
  else
    l2_block_time="${NO_NITRO_L2_BLOCK_TIME:-2}"
    l2_genesis_block=0
    l2_genesis_time=$(cast block 0 --json --rpc-url "$L1_RPC" | jq -r '.timestamp' | xargs printf '%d\n')
  fi

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
    "$source_config" >"$config_path.tmp"
  mv "$config_path.tmp" "$config_path"

  echo "Wrote deploy config: $config_path (anchor block $anchor_block, root $anchor_root)"
}

deploy_contracts() {
  local anchor_block anchor_root deployment_path
  if [ -f "$ROLLUP_CONFIG" ]; then
    anchor_block=$(resolve_anchor_block)
    anchor_root=$(output_root_at_block "$anchor_block")
  else
    anchor_block="${NO_NITRO_ANCHOR_BLOCK:-0}"
    anchor_root="${NO_NITRO_ANCHOR_ROOT:-$DEFAULT_ANCHOR_ROOT}"
  fi
  if ! [[ "$anchor_block" =~ ^[0-9]+$ ]]; then
    echo "ERROR: anchor block must be a decimal block number, got '$anchor_block'" >&2
    exit 1
  fi
  write_deploy_config "$anchor_block" "$anchor_root"

  echo "Deploying DeployDevNoNitro contracts from $CONTRACTS_DIR ..."
  mkdir -p "$CONTRACTS_DIR/deployments"
  (
    cd "$CONTRACTS_DIR"
    DEPLOY_CONFIG_PATH="$CONTRACTS_DIR/deploy-config/anvil-no-nitro.json" \
      forge script scripts/multiproof/DeployDevNoNitro.s.sol \
        --rpc-url "$L1_RPC" \
        --broadcast \
        --private-key "$OWNER_KEY"
  )

  deployment_path="$CONTRACTS_DIR/deployments/${L1_CHAIN_ID_VALUE}-dev-no-nitro.json"
  if [ ! -f "$deployment_path" ]; then
    echo "ERROR: expected deployment output not found at $deployment_path" >&2
    exit 1
  fi

  # DeployDevBase deploys ProtocolVersions internally; recover its address
  # from the AggregateVerifier so the ladder can be seeded below.
  local aggregate_verifier protocol_versions
  aggregate_verifier=$(jq -r '.AggregateVerifier' "$deployment_path")
  protocol_versions=$(call_first "$aggregate_verifier" "PROTOCOL_VERSIONS()(address)")

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
    }' \
    "$deployment_path" >"$ADDRESSES_FILE"

  echo "Saved addresses: $ADDRESSES_FILE"
}

# Timestamp for a fork name from the node-served rollup config. Matches the
# prover's schedule derivation: genesis-active forks (timestamp 0) map to the
# genesis timestamp, absent forks register as 0 (unscheduled).
rollup_timestamp() {
  local name="$1" path
  case "$name" in
    azul | beryl | cobalt) path=".base.${name}" ;;
    *) path=".${name}_time" ;;
  esac
  jq -r --arg path "$path" \
    '(getpath($path | ltrimstr(".") | split("."))) as $ts
     | if $ts == null then 0
       elif $ts == 0 then .genesis.l2_time
       else $ts end' \
    "$ROLLUP_CONFIG"
}

# Mirrors the node-served rollup config's fork timestamps onto the Anvil
# ProtocolVersions ladder: appends missing entries and rewrites changed ones.
# The rollup config is the same source the provers derive their schedule from,
# so games (activatedScheduleId at creation) and proof journals stay
# consistent even when the L2's schedule changes mid-session. With "quiet" as
# the first argument, prints nothing unless a change is applied.
sync_protocol_versions() {
  local mode="${1:-}" addr id ts onchain onchain_json
  load_fork_names || return 1
  addr=$(address_value ProtocolVersions)

  # Check the schedule read explicitly: the schedule-sync daemon calls this
  # under `|| true`, which suppresses errexit for the whole function body. An
  # unchecked failure (RPC hiccup, revert) would leave `onchain` empty and
  # re-append every fork via registerUpgrade, permanently shifting the
  # positional ladder that schedule ids are derived from.
  if ! onchain_json=$(cast call --json "$addr" "getSchedule()(uint64[])" --rpc-url "$L1_RPC"); then
    echo "WARN: getSchedule() failed at $addr via $L1_RPC; skipping sync" >&2
    return 1
  fi

  onchain=()
  while IFS= read -r ts; do
    onchain+=("$ts")
  done < <(jq -r '.[0][]' <<<"$onchain_json")

  if [ "$mode" != "quiet" ]; then
    echo "Syncing ${#FORK_NAMES[@]} upgrades into ProtocolVersions at $addr ..."
  fi

  local in_sync=1
  for ((id = 0; id < ${#FORK_NAMES[@]}; id++)); do
    ts=$(rollup_timestamp "${FORK_NAMES[$id]}")
    if [ "$id" -ge "${#onchain[@]}" ]; then
      echo "  id=$id ${FORK_NAMES[$id]}: registering timestamp $ts"
      if ! owner_send "$addr" "registerUpgrade(uint64,uint256)" "$ts" 0 >/dev/null; then
        echo "WARN: registerUpgrade failed for id=$id ${FORK_NAMES[$id]}; stopping sync" >&2
        return 1
      fi
      in_sync=0
    elif [ "${onchain[$id]}" != "$ts" ]; then
      echo "  id=$id ${FORK_NAMES[$id]}: timestamp ${onchain[$id]} -> $ts"
      if ! owner_send "$addr" "setTimestamp(uint256,uint64)" "$id" "$ts" >/dev/null; then
        echo "WARN: setTimestamp failed for id=$id ${FORK_NAMES[$id]}; stopping sync" >&2
        return 1
      fi
      in_sync=0
    fi
  done

  if [ "$in_sync" = "1" ] && [ "$mode" != "quiet" ]; then
    echo "ProtocolVersions ladder already in sync."
  fi
}

# Postgres in a container with no data volume: every container start is a
# fresh database, initialized from the checked-in migrations.
fresh_postgres() {
  echo "Starting Postgres with a fresh database ..."
  docker rm -f "$POSTGRES_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$POSTGRES_CONTAINER" \
    -p "$POSTGRES_PORT_VALUE:5432" \
    -e POSTGRES_DB=prover \
    -e POSTGRES_USER=prover \
    -e POSTGRES_PASSWORD=prover \
    -v "$REPO_ROOT/crates/proof/prover-service/db/migrations:/docker-entrypoint-initdb.d:ro" \
    postgres:17-alpine >/dev/null

  local deadline=$((SECONDS + 60))
  until docker exec "$POSTGRES_CONTAINER" pg_isready -U prover -d prover >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "ERROR: Postgres not ready after 60s (docker logs $POSTGRES_CONTAINER)" >&2
      exit 1
    fi
    sleep 1
  done
  echo "Postgres ready on port $POSTGRES_PORT_VALUE"
}

# Ready once the requester RPC answers with any JSON-RPC response (even an
# error): that proves the server is bound and the database pool came up.
wait_for_prover_service() {
  local deadline=$((SECONDS + 30))
  until curl -sf -m 2 -X POST -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"health","params":[]}' \
    "$PROVER_SERVICE_RPC" | jq -e '.jsonrpc' >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "ERROR: prover-service not responding after 30s at $PROVER_SERVICE_RPC (see $LOG_DIR/prover-service.log)" >&2
      exit 1
    fi
    sleep 1
  done
  echo "prover-service ready: rpc=$PROVER_SERVICE_RPC worker=$PROVER_SERVICE_WORKER_RPC"
}

register_signer() {
  local n="$1" registry signer registered
  registry=$(address_value TEEProverRegistry)
  signer=$(prover_signer_address "$n")

  registered=$(call_first "$registry" "isRegisteredSigner(address)(bool)" "$signer")
  if [ "$registered" = "true" ]; then
    echo "Local TEE signer $n already registered: $signer"
    return
  fi

  echo "Registering local TEE signer $n ($signer) on $registry ..."
  owner_send "$registry" "addDevSigner(address,bytes32)" "$signer" "$TEE_IMAGE_HASH"
}

run_anvil() {
  local args=(--chain-id "$L1_CHAIN_ID_VALUE" --port "$ANVIL_PORT")
  if [ -n "$CALLER_ANVIL_FORK_URL" ]; then
    # Mine only on transactions so the proof's origin remains inside the
    # verifier's 256-block blockhash window while local proving runs.
    args+=(--fork-url "$CALLER_ANVIL_FORK_URL")
  else
    args+=(--block-time 1 --slots-in-an-epoch 1)
  fi
  exec anvil "${args[@]}"
}

run_prover_service() {
  exec env \
    POSTGRES_HOST=localhost \
    POSTGRES_PORT="$POSTGRES_PORT_VALUE" \
    POSTGRES_DB=prover \
    POSTGRES_USER=prover \
    POSTGRES_PASSWORD=prover \
    POSTGRES_SSLMODE=disable \
    "$REPO_ROOT/target/debug/base-prover-service" \
    --rpc-listen-addr "127.0.0.1:$PROVER_SERVICE_RPC_PORT_VALUE" \
    --worker-rpc-listen-addr "127.0.0.1:$PROVER_SERVICE_WORKER_RPC_PORT_VALUE"
}

run_nitro_local() {
  local n="$1"
  exec env BASE_ENCLAVE_SIGNER_KEY="$(prover_signer_key "$n")" \
    "$REPO_ROOT/target/debug/base-prover-nitro-host" \
    local \
    --l1-eth-url "$L1_PROOF_RPC" \
    --l2-eth-url "$L2_ETH_RPC" \
    --l2-node-url "$L2_NODE_RPC" \
    --l1-beacon-url "$L1_BEACON_RPC" \
    --l2-chain-id "$L2_CHAIN_ID_VALUE" \
    --prover-service-endpoint "$PROVER_SERVICE_WORKER_RPC"
}

# Daemon loop: re-fetches the node-served rollup config every
# SCHEDULE_SYNC_INTERVAL_SECS and mirrors any schedule change onto the Anvil
# ProtocolVersions. Skips cycles while the L2 node is unreachable.
run_schedule_sync() {
  local interval="${SCHEDULE_SYNC_INTERVAL_SECS:-30}"
  while true; do
    if l2_node_available; then
      if cast rpc optimism_rollupConfig --rpc-url "$L2_NODE_RPC" 2>/dev/null |
        jq . >"$ROLLUP_CONFIG.tmp" 2>/dev/null; then
        mv "$ROLLUP_CONFIG.tmp" "$ROLLUP_CONFIG"
        sync_protocol_versions quiet || true
      fi
    fi
    sleep "$interval"
  done
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
    --tee-image-hash "$TEE_IMAGE_HASH" \
    --health.port "$PROPOSER_HEALTH_PORT_VALUE" \
    --private-key "$TEE_PROPOSER_KEY"
}

start_daemon() {
  local name="$1"
  shift

  local log_file="$LOG_DIR/$name.log"
  mkdir -p "$LOG_DIR" "$PID_DIR"
  echo "Starting $name; log: $log_file"
  if command -v setsid >/dev/null 2>&1; then
    setsid "$SCRIPT_DIR/anvil-no-nitro.sh" "$@" >"$log_file" 2>&1 &
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' \
      "$SCRIPT_DIR/anvil-no-nitro.sh" "$@" >"$log_file" 2>&1 &
  else
    "$SCRIPT_DIR/anvil-no-nitro.sh" "$@" >"$log_file" 2>&1 &
  fi
  echo $! >"$PID_DIR/$name.pid"
}

daemon_names() {
  DAEMON_NAMES=()
  local pid_file
  for pid_file in "$PID_DIR"/*.pid; do
    [ -f "$pid_file" ] || continue
    DAEMON_NAMES+=("$(basename "$pid_file" .pid)")
  done
}

stop_daemons() {
  daemon_names
  [ "${#DAEMON_NAMES[@]}" -gt 0 ] || return 0

  local name pid
  for name in "${DAEMON_NAMES[@]}"; do
    pid=$(cat "$PID_DIR/$name.pid")
    if kill -0 "$pid" >/dev/null 2>&1; then
      echo "Stopping $name (pid $pid) ..."
      kill -TERM "-$pid" >/dev/null 2>&1 || kill -TERM "$pid" >/dev/null 2>&1 || true
    fi
  done

  sleep 2

  for name in "${DAEMON_NAMES[@]}"; do
    pid=$(cat "$PID_DIR/$name.pid")
    if kill -0 "$pid" >/dev/null 2>&1; then
      kill -KILL "-$pid" >/dev/null 2>&1 || kill -KILL "$pid" >/dev/null 2>&1 || true
    fi
    rm -f "$PID_DIR/$name.pid"
  done
}

cmd_up() {
  require_tools cast forge jq git docker cargo curl
  [ -n "$CALLER_L1_RPC_URL" ] || require_tools anvil
  stop_daemons
  rm -rf "$STATE_DIR/logs" "$STATE_DIR/pids" "$ROLLUP_CONFIG" "$ADDRESSES_FILE"
  mkdir -p "$STATE_DIR"

  # Build all native binaries up front so failures surface before anything starts.
  echo "Building base-prover-service, base-prover-nitro-host (local), and base-proposer ..."
  run_with_native_env cargo build \
    --package base-prover-service-bin --bin base-prover-service
  run_with_native_env cargo build \
    --package base-prover-nitro-host --bin base-prover-nitro-host --features local
  run_with_native_env cargo build --package base-proposer-bin --bin base-proposer

  # 1. Start Anvil (optionally forked from the origin L1) or use the caller's
  #    existing canonical L1, then deploy the dev contracts.
  if [ -n "$CALLER_L1_RPC_URL" ]; then
    echo "Using existing L1 at $L1_RPC"
  elif [ -n "$CALLER_ANVIL_FORK_URL" ]; then
    echo "Starting Anvil fork of $CALLER_ANVIL_FORK_URL at $L1_RPC"
    start_daemon "anvil" _anvil
  else
    start_daemon "anvil" _anvil
  fi
  wait_for_l1
  if l2_node_available; then
    echo "Live L2 node detected at $L2_NODE_RPC; seeding anchor state and schedule from it."
    fetch_rollup_config
    validate_l1_for_l2
  else
    echo "No L2 node at $L2_NODE_RPC; deploying with placeholder anchor state." \
      "Provers will idle; re-run 'up' with live endpoints before proving."
  fi
  prepare_checkout
  ensure_contract_deps
  deploy_contracts
  if [ -f "$ROLLUP_CONFIG" ]; then
    sync_protocol_versions
    # Keep the ladder mirroring the L2's schedule for the whole session so
    # later registrations on the L2 side don't desync games from proofs.
    start_daemon "schedule-sync" _schedule-sync
  fi

  # 2. prover-service with a fresh database.
  fresh_postgres
  start_daemon "prover-service" _prover-service
  wait_for_prover_service

  # 3. The local nitro provers.
  local n
  for ((n = 1; n <= NITRO_PROVERS; n++)); do
    start_daemon "nitro-prover-$n" _nitro-local "$n"
  done

  # 4. Register their signers on DevTEEProverRegistry.
  for ((n = 1; n <= NITRO_PROVERS; n++)); do
    register_signer "$n"
  done

  # 5. The proposer.
  start_daemon "proposer" _proposer

  echo ""
  echo "Anvil no-Nitro proving stack is up:"
  echo "  contract L1:        $L1_RPC (chain $L1_CHAIN_ID_VALUE)"
  echo "  proof-input L1:     $L1_PROOF_RPC"
  echo "  ProtocolVersions:   $(address_value ProtocolVersions)"
  echo "  TEEProverRegistry:  $(address_value TEEProverRegistry)"
  echo "  DisputeGameFactory: $(address_value DisputeGameFactory)"
  echo "  prover-service:     $PROVER_SERVICE_RPC (worker: $PROVER_SERVICE_WORKER_RPC)"
  echo "  nitro provers:      $NITRO_PROVERS (local mode)"
  echo "  logs:               $LOG_DIR/"
  echo ""
  echo "Check on it with 'just anvil-no-nitro status'."
  echo "Stop it with 'just anvil-no-nitro down'."
}

cmd_sync() {
  require_tools cast jq cargo
  if [ ! -f "$ADDRESSES_FILE" ]; then
    echo "ERROR: no deployment found at $ADDRESSES_FILE; run 'up' first" >&2
    exit 1
  fi
  # A manual sync racing the schedule-sync daemon can double-registerUpgrade
  # the same fork (both read the schedule before either writes), corrupting
  # the positional ladder. The daemon already syncs every 30s, so refuse.
  local sync_pid_file="$PID_DIR/schedule-sync.pid" sync_pid
  if [ -f "$sync_pid_file" ] && sync_pid=$(cat "$sync_pid_file") &&
    kill -0 "$sync_pid" >/dev/null 2>&1; then
    echo "ERROR: schedule-sync daemon is running (pid $sync_pid) and already" \
      "syncs periodically; stop it with 'down' before syncing manually" >&2
    exit 1
  fi
  if ! l2_node_available; then
    echo "ERROR: no L2 node reachable at $L2_NODE_RPC" >&2
    exit 1
  fi
  fetch_rollup_config
  validate_l1_for_l2
  sync_protocol_versions
}

cmd_down() {
  stop_daemons
  if command -v docker >/dev/null 2>&1; then
    docker rm -f "$POSTGRES_CONTAINER" >/dev/null 2>&1 || true
  fi
  echo "Anvil no-Nitro proving stack stopped."
}

parse_game_at_index() {
  local output="$1" line_count
  line_count=$(printf '%s\n' "$output" | sed '/^[[:space:]]*$/d' | wc -l | tr -d '[:space:]')
  if [ "$line_count" -ge 3 ]; then
    local game_type timestamp proxy
    game_type=$(printf '%s\n' "$output" | sed -n '1p' | first_word)
    timestamp=$(printf '%s\n' "$output" | sed -n '2p' | first_word)
    proxy=$(printf '%s\n' "$output" | sed -n '3p' | first_word)
    echo "$game_type $timestamp $proxy"
  else
    printf '%s\n' "$output" | tr '(),' '   ' | awk '{ print $1, $2, $3 }'
  fi
}

latest_game_for_type() {
  local factory count idx game_info parsed game_type timestamp proxy
  factory=$(address_value DisputeGameFactory)
  count=$(call_first "$factory" "gameCount()(uint256)")
  if [ "$count" = "0" ]; then
    return 1
  fi

  idx=$((count - 1))
  while [ "$idx" -ge 0 ]; do
    game_info=$(cast call "$factory" "gameAtIndex(uint256)(uint32,uint64,address)" "$idx" --rpc-url "$L1_RPC")
    parsed=$(parse_game_at_index "$game_info")
    game_type=$(printf '%s\n' "$parsed" | awk '{ print $1 }')
    timestamp=$(printf '%s\n' "$parsed" | awk '{ print $2 }')
    proxy=$(printf '%s\n' "$parsed" | awk '{ print $3 }')
    if [ "$game_type" = "$GAME_TYPE" ]; then
      echo "$idx $timestamp $proxy"
      return 0
    fi
    idx=$((idx - 1))
  done

  return 1
}

status_label() {
  case "$1" in
    0) echo "InProgress (0)" ;;
    1) echo "ChallengerWins (1)" ;;
    2) echo "DefenderWins (2)" ;;
    *) echo "Unknown ($1)" ;;
  esac
}

cmd_status() {
  require_tools cast jq

  echo "Daemons"
  daemon_names
  if [ "${#DAEMON_NAMES[@]}" -eq 0 ]; then
    echo "  none running"
  else
    local name pid state
    for name in "${DAEMON_NAMES[@]}"; do
      pid=$(cat "$PID_DIR/$name.pid")
      state="dead (see $LOG_DIR/$name.log)"
      kill -0 "$pid" >/dev/null 2>&1 && state="running"
      echo "  $name: pid $pid $state"
    done
  fi
  echo ""

  if [ ! -f "$ADDRESSES_FILE" ]; then
    echo "No deployment found at $ADDRESSES_FILE"
    return
  fi

  echo "Addresses"
  jq -r 'to_entries[] | select(.value | type == "string") | "  \(.key): \(.value)"' "$ADDRESSES_FILE"
  echo ""

  local protocol_versions
  protocol_versions=$(address_value ProtocolVersions)
  echo "ProtocolVersions scheduleId: $(call_first "$protocol_versions" "scheduleId()(bytes32)")"
  echo ""

  local registry n signer
  registry=$(address_value TEEProverRegistry)
  echo "Local TEE signers (expectedImage: $(call_first "$registry" "getExpectedImageHash()(bytes32)"))"
  for ((n = 1; n <= NITRO_PROVERS; n++)); do
    signer=$(prover_signer_address "$n")
    echo "  prover $n: $signer" \
      "registered=$(call_first "$registry" "isRegisteredSigner(address)(bool)" "$signer")" \
      "valid=$(call_first "$registry" "isValidSigner(address)(bool)" "$signer")"
  done
  echo ""

  local latest idx timestamp game status
  if ! latest=$(latest_game_for_type); then
    echo "No dispute games found for game type $GAME_TYPE"
    return
  fi

  idx=$(printf '%s\n' "$latest" | awk '{ print $1 }')
  timestamp=$(printf '%s\n' "$latest" | awk '{ print $2 }')
  game=$(printf '%s\n' "$latest" | awk '{ print $3 }')
  status=$(call_first "$game" "status()(uint8)")

  echo "Latest game for type $GAME_TYPE"
  echo "  index:      $idx"
  echo "  address:    $game"
  echo "  createdAt:  $timestamp"
  echo "  scheduleId: $(call_first "$game" "scheduleId()(bytes32)")"
  echo "  status:     $(status_label "$status")"
  echo "  proofCount: $(call_first "$game" "proofCount()(uint8)")"
  echo "  teeProver:  $(call_first "$game" "teeProver()(address)")"
  echo "  rootClaim:  $(call_first "$game" "rootClaim()(bytes32)")"
  echo "  l2Block:    $(call_first "$game" "l2SequenceNumber()(uint256)")"
}

cmd_logs() {
  daemon_names
  if [ "${#DAEMON_NAMES[@]}" -eq 0 ]; then
    echo "No daemons running (no pid files in $PID_DIR)" >&2
    exit 1
  fi
  local name log_files=()
  for name in "${DAEMON_NAMES[@]}"; do
    [ -f "$LOG_DIR/$name.log" ] && log_files+=("$LOG_DIR/$name.log")
  done
  exec tail -n 50 -F "${log_files[@]}"
}

main() {
  local command="${1:-}"
  case "$command" in
    up) cmd_up ;;
    down) cmd_down ;;
    sync) cmd_sync ;;
    status) cmd_status ;;
    logs) cmd_logs ;;
    _anvil) run_anvil ;;
    _schedule-sync) run_schedule_sync ;;
    _prover-service) run_prover_service ;;
    _nitro-local) run_nitro_local "${2:?prover number required}" ;;
    _proposer) run_proposer ;;
    *)
      usage
      exit 1
      ;;
  esac
}

main "$@"
