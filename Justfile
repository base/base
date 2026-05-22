# On macOS, skip risc0-sys kernel compilation for check/clippy commands.
# The kernels require Xcode (Metal) on macOS but are only needed for linking
# (cargo build), not for type-checking (cargo check/clippy). CI builds run
# on Linux where CPU kernels compile without issue.
_skip_kernels := if os() == "macos" { "RISC0_SKIP_BUILD_KERNELS=1" } else { "" }

set positional-arguments := true

mod tee 'crates/proof/tee'
mod actions 'actions'
# Docker-based local devnet management
mod devnet 'etc/docker'
# Load testing for networks
mod load-test 'crates/infra/load-tests'
# Formatting, clippy, udeps, and deny checks
mod check 'etc/just/check.just'
# Cargo build targets and contract compilation
mod build 'etc/just/build.just'
# SP1 / succinct ELF builds and proving helpers
mod succinct 'etc/just/succinct.just'
# Standalone ZK prover management and gRPC helpers
mod zk-prover 'etc/just/zk-prover.just'

alias t := test
alias f := fix
alias be := benches
alias c := clean
alias h := hack
alias wt := watch-test
alias wc := watch-check
alias ldc := load-test-devnet-continuous

# Default to display help menu
default:
    @just --list

# Load test devnet in continuous mode (Ctrl-C to stop)
load-test-devnet-continuous:
    just load-test devnet-continuous

# Runs the specs docs locally
specs:
    cd docs/specs && bun ci && bun dev

# One-time project setup: installs tooling and builds test contracts
setup:
    #!/usr/bin/env bash
    set -euo pipefail

    OS="$(uname -s)"
    ARCH="$(uname -m)"

    # ── Install fast linker ──
    if [[ "$OS" == "Darwin" ]]; then
        if ! brew list lld &>/dev/null; then
            echo "Installing lld linker for faster builds..."
            brew install lld
        fi
        # Verify lld is reachable at the path .cargo/config.toml expects
        if [[ "$ARCH" == "arm64" ]]; then
            LLD="/opt/homebrew/opt/lld/bin/ld64.lld"
        else
            LLD="/usr/local/opt/lld/bin/ld64.lld"
        fi
        if [[ ! -x "$LLD" ]]; then
            echo "ERROR: lld not found at $LLD"
            echo "Try: brew install lld"
            exit 1
        fi
        echo "Found lld at $LLD"
    elif [[ "$OS" == "Linux" ]]; then
        if ! command -v mold &>/dev/null; then
            echo "mold not found. Install it for faster builds:"
            echo "  Ubuntu/Debian: sudo apt-get install -y mold"
            echo "  Fedora:        sudo dnf install mold"
            echo "  Arch:          sudo pacman -S mold"
            exit 1
        fi
        echo "Found mold at $(command -v mold)"
    fi

    just build contracts
    echo "Setup complete!"

# Runs all ci checks
ci: fix check::all test lychee zepter check::no-std check::no-std-proof

# Runs ci checks with tests scoped to crates affected by changes
pr: fix check::format check::udeps check::clippy check::deny lychee zepter check::no-std check::no-std-proof test-affected

# Performs lychee checks, installing the lychee command if necessary
lychee:
    @command -v lychee >/dev/null 2>&1 || cargo install lychee
    lychee --config ./lychee.toml .

# Fixes formatting and clippy issues
fix: build::contracts format-fix clippy-fix zepter-fix

# Runs zepter feature checks, installing zepter if necessary
zepter:
    @command -v zepter >/dev/null 2>&1 || cargo install zepter
    zepter --version
    zepter format features
    zepter

# Fixes zepter feature formatting.
zepter-fix:
    @command -v zepter >/dev/null 2>&1 || cargo install zepter
    zepter format features --fix

# Installs cargo-nextest if not present
install-nextest:
    @command -v cargo-nextest >/dev/null 2>&1 || cargo install cargo-nextest --locked

# Runs tests across workspace with all features enabled (excludes devnet)
test: install-nextest build::contracts build::elfs
    cargo nextest run --workspace --all-features --exclude devnet --no-fail-fast

# Runs tests only for crates affected by changes vs main (excludes devnet)
test-affected base="main": install-nextest build::contracts build::elfs
    #!/usr/bin/env bash
    set -euo pipefail
    pkg_args_output="$(python3 etc/scripts/local/affected-crates.py {{ base }} --exclude devnet --cargo-args)"
    pkg_args=()
    while IFS= read -r line; do
        [ -n "$line" ] && pkg_args+=("$line")
    done <<< "$pkg_args_output"
    if [ "${#pkg_args[@]}" -eq 0 ]; then
        echo "No affected crates to test."
        exit 0
    fi
    echo "Testing affected crates:${pkg_args[*]}"
    cargo nextest run --all-features "${pkg_args[@]}"

# Runs tests with ci profile for minimal disk usage
test-ci: install-nextest build::contracts build::elfs
    cargo nextest run -P ci --locked --workspace --all-features --exclude devnet --cargo-profile ci

# Runs tests only for affected crates with ci profile (for PRs)
test-affected-ci base="main": install-nextest build::contracts build::elfs
    #!/usr/bin/env bash
    set -euo pipefail
    pkg_args_output="$(python3 etc/scripts/local/affected-crates.py {{ base }} --exclude devnet --cargo-args)"
    pkg_args=()
    while IFS= read -r line; do
        [ -n "$line" ] && pkg_args+=("$line")
    done <<< "$pkg_args_output"
    if [ "${#pkg_args[@]}" -eq 0 ]; then
        echo "No affected crates to test."
        exit 0
    fi
    echo "Testing affected crates:${pkg_args[*]}"
    cargo nextest run -P ci --locked --all-features --cargo-profile ci "${pkg_args[@]}" || {
        code=$?
        if [ $code -eq 4 ]; then
            echo "No tests to run."
            exit 0
        fi
        exit $code
    }

# Runs cargo hack against the workspace
hack:
    cargo hack check --feature-powerset --no-dev-deps

# Fixes any formatting issues
format-fix:
    {{_skip_kernels}} BASE_SUCCINCT_ELF_STUB=1 cargo fix --allow-dirty --allow-staged --workspace
    cargo +nightly fmt --all

# Fixes any clippy issues
clippy-fix:
    {{_skip_kernels}} BASE_SUCCINCT_ELF_STUB=1 cargo clippy --workspace --all-features --all-targets --fix --allow-dirty --allow-staged

# Cleans the workspace
clean:
    cargo clean

# Watches tests
watch-test: build::contracts
    cargo watch -x test

# Watches checks
watch-check:
    cargo watch -x "fmt --all -- --check" -x "clippy --all-features --all-targets -- -D warnings" -x test

# Runs all benchmarks
benches:
    @just bench-flashblocks
    @just bench-proof-mpt

# Runs flashblocks pending state benchmarks
bench-flashblocks:
    cargo bench -p base-flashblocks --bench pending_state

# Runs MPT trie node benchmarks
bench-proof-mpt:
    cargo bench -p base-proof-mpt --bench trie_node

# Run basectl TUI dashboard
basectl:
    cargo run -p basectl --release

# Run the manual ZK fork dispute test.
#
# Reads fork config from crates/proof/challenge/fork-tests/zk-fork-dispute/<chain>.yaml.
# Supported chains: mainnet, sepolia, zeronet.
#
# Config keys and matching env overrides:
#   BASE_ZK_FORK_L1_RPC_URL
#   BASE_ZK_FORK_L1_FORK_URL
#   BASE_ZK_FORK_L2_RPC_URL
#   BASE_ZK_FORK_ROLLUP_RPC_URL
#   BASE_ZK_FORK_L1_BEACON_RPC_URL
#   BASE_ZK_FORK_PROVER_MODE
#   BASE_ZK_FORK_PROVER_L2_NODE_RPC_URL
#   BASE_ZK_FORK_PROVER_GRPC_URL
#   BASE_ZK_FORK_DISPUTE_GAME_FACTORY
#   BASE_ZK_FORK_GAME_ADDRESS / BASE_ZK_FORK_GAME_INDEX
#   BASE_ZK_FORK_PRIVATE_KEY

# Run the manual ZK fork dispute test.
#
# If no local fork is already running, this starts an Anvil fork.
# Pass game as the third arg, or set game_address/game_index in the chain YAML.
# Use an empty game arg when passing game_index:
#   just zk-fork-dispute sepolia nullify '' 123
zk-fork-dispute chain='sepolia' intent='' game='' game_index='' invalid_index='':
    #!/usr/bin/env bash
    set -euo pipefail

    repo_root="{{ justfile_directory() }}"
    anvil_pid=''
    prover_pid=''
    prover_log_tail_pid=''
    postgres_started_by_recipe=''
    cleanup() {
      if [[ -n "$prover_log_tail_pid" ]]; then
        kill "$prover_log_tail_pid" >/dev/null 2>&1 || true
      fi
      if [[ -n "$prover_pid" ]]; then
        kill "$prover_pid" >/dev/null 2>&1 || true
      fi
      if [[ -n "$anvil_pid" ]]; then
        kill "$anvil_pid" >/dev/null 2>&1 || true
      fi
      if [[ -n "$postgres_started_by_recipe" ]]; then
        docker stop "$postgres_started_by_recipe" >/dev/null 2>&1 || true
      fi
    }
    trap cleanup EXIT

    case "{{chain}}" in
      mainnet|sepolia|zeronet) ;;
      *)
        echo "chain must be one of: mainnet, sepolia, zeronet" >&2
        exit 1
        ;;
    esac

    export BASE_ZK_FORK_CHAIN="{{chain}}"
    config_file="$repo_root/crates/proof/challenge/fork-tests/zk-fork-dispute/{{chain}}.yaml"
    yaml_config_value() {
      local key="$1"
      if [[ ! -f "$config_file" ]]; then
        return 0
      fi
      awk -v key="$key" '
        $1 == key ":" {
          value = $0
          sub("^[^:]+:[[:space:]]*", "", value)
          gsub(/^"|"$/, "", value)
          print value
          exit
        }
      ' "$config_file"
    }
    config_l1_fork_url="$(yaml_config_value l1_fork_url)"
    config_l2_rpc_url="$(yaml_config_value l2_rpc_url)"
    config_rollup_rpc_url="$(yaml_config_value rollup_rpc_url)"
    config_prover_l2_node_rpc_url="$(yaml_config_value prover_l2_node_rpc_url)"
    config_l1_beacon_rpc_url="$(yaml_config_value l1_beacon_rpc_url)"
    config_prover_grpc_url="$(yaml_config_value prover_grpc_url)"

    : "${BASE_ZK_FORK_L1_RPC_URL:=http://127.0.0.1:18545}"
    export BASE_ZK_FORK_L1_RPC_URL
    case "{{chain}}" in
      mainnet)
        : "${BASE_ZK_FORK_L1_FORK_URL:=${config_l1_fork_url:?set l1_fork_url in mainnet.yaml or BASE_ZK_FORK_L1_FORK_URL}}"
        : "${BASE_ZK_FORK_L2_RPC_URL:=${config_l2_rpc_url:?set l2_rpc_url in mainnet.yaml or BASE_ZK_FORK_L2_RPC_URL}}"
        : "${BASE_ZK_FORK_ROLLUP_RPC_URL:=${config_rollup_rpc_url:?set rollup_rpc_url in mainnet.yaml or BASE_ZK_FORK_ROLLUP_RPC_URL}}"
        : "${BASE_ZK_FORK_L1_BEACON_RPC_URL:=${config_l1_beacon_rpc_url:-${ETH_MAINNET_BEACON_RPC:?set l1_beacon_rpc_url in mainnet.yaml, ETH_MAINNET_BEACON_RPC, or BASE_ZK_FORK_L1_BEACON_RPC_URL}}}"
        ;;
      sepolia)
        : "${BASE_ZK_FORK_L1_FORK_URL:=${config_l1_fork_url:?set l1_fork_url in sepolia.yaml or BASE_ZK_FORK_L1_FORK_URL}}"
        : "${BASE_ZK_FORK_L2_RPC_URL:=${config_l2_rpc_url:?set l2_rpc_url in sepolia.yaml or BASE_ZK_FORK_L2_RPC_URL}}"
        : "${BASE_ZK_FORK_ROLLUP_RPC_URL:=${config_rollup_rpc_url:?set rollup_rpc_url in sepolia.yaml or BASE_ZK_FORK_ROLLUP_RPC_URL}}"
        : "${BASE_ZK_FORK_L1_BEACON_RPC_URL:=${config_l1_beacon_rpc_url:-${ETH_SEPOLIA_BEACON_RPC:?set l1_beacon_rpc_url in sepolia.yaml, ETH_SEPOLIA_BEACON_RPC, or BASE_ZK_FORK_L1_BEACON_RPC_URL}}}"
        ;;
      zeronet)
        : "${BASE_ZK_FORK_L1_FORK_URL:=${config_l1_fork_url:?set l1_fork_url in zeronet.yaml or BASE_ZK_FORK_L1_FORK_URL}}"
        : "${BASE_ZK_FORK_L2_RPC_URL:=${config_l2_rpc_url:?set l2_rpc_url in zeronet.yaml or BASE_ZK_FORK_L2_RPC_URL}}"
        : "${BASE_ZK_FORK_ROLLUP_RPC_URL:=${config_rollup_rpc_url:?set rollup_rpc_url in zeronet.yaml or BASE_ZK_FORK_ROLLUP_RPC_URL}}"
        : "${BASE_ZK_FORK_L1_BEACON_RPC_URL:=${config_l1_beacon_rpc_url:?set l1_beacon_rpc_url in zeronet.yaml or BASE_ZK_FORK_L1_BEACON_RPC_URL}}"
        ;;
    esac
    : "${BASE_ZK_FORK_PROVER_L2_NODE_RPC_URL:=${config_prover_l2_node_rpc_url:-$BASE_ZK_FORK_ROLLUP_RPC_URL}}"
    export BASE_ZK_FORK_L1_FORK_URL
    export BASE_ZK_FORK_L2_RPC_URL
    export BASE_ZK_FORK_ROLLUP_RPC_URL
    export BASE_ZK_FORK_PROVER_L2_NODE_RPC_URL
    export BASE_ZK_FORK_L1_BEACON_RPC_URL

    anvil_pid=''
    if ! curl -fsS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' "$BASE_ZK_FORK_L1_RPC_URL" >/dev/null 2>&1; then
      command -v anvil >/dev/null || { echo "anvil is required to start the L1 fork" >&2; exit 1; }
      anvil_log="$(mktemp -t zk-fork-anvil.XXXXXX.log)"
      anvil --fork-url "$BASE_ZK_FORK_L1_FORK_URL" --host 127.0.0.1 --port 18545 >"$anvil_log" 2>&1 &
      anvil_pid="$!"
      for _ in {1..60}; do
        if curl -fsS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' "$BASE_ZK_FORK_L1_RPC_URL" >/dev/null 2>&1; then
          break
        fi
        sleep 1
      done
      curl -fsS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' "$BASE_ZK_FORK_L1_RPC_URL" >/dev/null 2>&1 || {
        echo "anvil did not start; log: $anvil_log" >&2
        exit 1
      }
    fi

    if [[ -n "${BASE_ZK_FORK_PRIVATE_KEY:-}" ]] && command -v cast >/dev/null; then
      signer_address="$(cast wallet address --private-key "$BASE_ZK_FORK_PRIVATE_KEY")"
      curl -fsS -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"anvil_setBalance\",\"params\":[\"$signer_address\",\"0x3635C9ADC5DEA00000\"]}" \
        "$BASE_ZK_FORK_L1_RPC_URL" >/dev/null 2>&1 || true
    fi

    if [[ -n "{{intent}}" ]]; then
      export BASE_ZK_FORK_DISPUTE_INTENT="{{intent}}"
    fi
    : "${BASE_ZK_FORK_PROVER_GRPC_URL:=${config_prover_grpc_url:-http://localhost:9090}}"
    export BASE_ZK_FORK_PROVER_GRPC_URL

    prover_endpoint="${BASE_ZK_FORK_PROVER_GRPC_URL#http://}"
    prover_endpoint="${prover_endpoint#https://}"
    prover_endpoint="${prover_endpoint%%/*}"
    prover_host="${prover_endpoint%%:*}"
    prover_port="${prover_endpoint##*:}"
    if [[ "$prover_port" == "$prover_host" ]]; then
      case "$BASE_ZK_FORK_PROVER_GRPC_URL" in
        https://*) prover_port=443 ;;
        *) prover_port=80 ;;
      esac
    fi

    is_local_prover=''
    case "$prover_host" in
      localhost|127.*) is_local_prover=1 ;;
    esac
    prover_listening() {
      : >"/dev/tcp/$prover_host/$prover_port"
    } >/dev/null 2>&1

    if [[ "${BASE_ZK_FORK_AUTO_PROVER:-true}" == "true" && -n "$is_local_prover" ]] \
      && ! prover_listening; then
      command -v docker >/dev/null || { echo "docker is required to auto-start local prover Postgres" >&2; exit 1; }

      prover_mode="${BASE_ZK_FORK_PROVER_MODE:-${SP1_PROVER:-cluster}}"
      prover_l2_node_rpc="${BASE_ZK_FORK_PROVER_L2_NODE_RPC_URL:-${BASE_ZK_FORK_ROLLUP_RPC_URL:-}}"
      prover_l1_beacon_rpc="${BASE_ZK_FORK_L1_BEACON_RPC_URL:-}"
      if [[ "$prover_mode" == "cluster" ]]; then
        : "${SP1_CLUSTER_API_ENDPOINT:?set SP1_CLUSTER_API_ENDPOINT for cluster prover mode}"
        : "${CLI_S3_BUCKET:=protocols-base-proofs-sp1-cluste-sp1-cluste-s3-697a3c}"
        : "${CLI_S3_REGION:=us-east-1}"
        : "${AWS_ENDPOINT_URL:=https://s3.us-east-1.amazonaws.com}"
      fi
      if [[ "$prover_mode" == "mock" ]]; then
        : "${prover_l2_node_rpc:=$BASE_ZK_FORK_L2_RPC_URL}"
        : "${prover_l1_beacon_rpc:=$BASE_ZK_FORK_L1_RPC_URL}"
      else
        if [[ -z "$prover_l2_node_rpc" ]]; then
          echo "set BASE_ZK_FORK_ROLLUP_RPC_URL or BASE_ZK_FORK_PROVER_L2_NODE_RPC_URL for the local prover rollup RPC" >&2
          exit 1
        fi
        if [[ -z "$prover_l1_beacon_rpc" ]]; then
          echo "set ETH_SEPOLIA_BEACON_RPC or BASE_ZK_FORK_L1_BEACON_RPC_URL for local prover witness generation" >&2
          exit 1
        fi
      fi

      postgres_container="${BASE_ZK_FORK_PROVER_POSTGRES_CONTAINER:-zk-prover-fork-postgres}"
      postgres_port="${BASE_ZK_FORK_PROVER_POSTGRES_PORT:-15432}"
      postgres_data_dir="${BASE_ZK_FORK_PROVER_POSTGRES_DATA_DIR:-$repo_root/.devnet/zk-prover-fork/postgres}"
      l1_config_dir="${BASE_ZK_FORK_PROVER_L1_CONFIG_DIR:-$repo_root/.devnet/zk-prover-fork/configs/L1}"
      l2_config_dir="${BASE_ZK_FORK_PROVER_L2_CONFIG_DIR:-$repo_root/.devnet/zk-prover-fork/configs/L2}"

      mkdir -p "$postgres_data_dir" "$l1_config_dir" "$l2_config_dir"
      if [[ -z "$(docker ps -q -f "name=^/${postgres_container}$")" ]]; then
        if [[ -n "$(docker ps -aq -f "name=^/${postgres_container}$")" ]]; then
          docker start "$postgres_container" >/dev/null
        else
          docker run -d \
            --name "$postgres_container" \
            -p "127.0.0.1:${postgres_port}:5432" \
            -e POSTGRES_DB=prover \
            -e POSTGRES_USER=prover \
            -e POSTGRES_PASSWORD=prover \
            -v "$postgres_data_dir:/var/lib/postgresql/data" \
            -v "$repo_root/crates/proof/zk/db/migrations:/docker-entrypoint-initdb.d:ro" \
            postgres:17-alpine >/dev/null
        fi
        postgres_started_by_recipe="$postgres_container"
      fi

      for _ in {1..120}; do
        if docker exec "$postgres_container" pg_isready -U prover -d prover >/dev/null 2>&1; then
          break
        fi
        sleep 1
      done
      docker exec "$postgres_container" pg_isready -U prover -d prover >/dev/null

      prover_log="$(mktemp -t zk-fork-prover.XXXXXX.log)"
      listen_host="$prover_host"
      if [[ "$listen_host" == "localhost" ]]; then
        listen_host="127.0.0.1"
      fi
      (
        export SP1_PROVER="$prover_mode"
        export PROXY_ENABLE=false
        export GRPC_LISTEN_ADDR="${listen_host}:${prover_port}"
        export BASE_CONSENSUS_ADDRESS="$prover_l2_node_rpc"
        export L1_NODE_ADDRESS="$BASE_ZK_FORK_L1_RPC_URL"
        export L1_BEACON_ADDRESS="$prover_l1_beacon_rpc"
        export L2_NODE_ADDRESS="$BASE_ZK_FORK_L2_RPC_URL"
        export DEFAULT_SEQUENCE_WINDOW="${DEFAULT_SEQUENCE_WINDOW:-50}"
        export POSTGRES_HOST=127.0.0.1
        export POSTGRES_PORT="$postgres_port"
        export POSTGRES_DB=prover
        export POSTGRES_USER=prover
        export POSTGRES_PASSWORD=prover
        export POSTGRES_SSLMODE=disable
        export SP1_CLUSTER_API_ENDPOINT="${SP1_CLUSTER_API_ENDPOINT:-}"
        export CLI_S3_BUCKET="${CLI_S3_BUCKET:-}"
        export CLI_S3_REGION="${CLI_S3_REGION:-}"
        export AWS_ENDPOINT_URL="${AWS_ENDPOINT_URL:-}"
        export L1_CONFIG_DIR="$l1_config_dir"
        export L2_CONFIG_DIR="$l2_config_dir"
        export OTEL_SERVICE_NAME=base-prover-zk-fork
        cargo run -p base-prover-zk --bin base-prover-zk
      ) >"$prover_log" 2>&1 &
      prover_pid="$!"

      tail -f "$prover_log" &
      prover_log_tail_pid="$!"
      echo "started local zk prover for fork test"
      prover_ready_timeout_secs="${BASE_ZK_FORK_PROVER_READY_TIMEOUT_SECS:-1800}"
      for _ in $(seq 1 "$prover_ready_timeout_secs"); do
        if prover_listening; then
          break
        fi
        if ! kill -0 "$prover_pid" >/dev/null 2>&1; then
          echo "local zk prover exited before becoming ready; log: $prover_log" >&2
          exit 1
        fi
        sleep 1
      done
      prover_listening || {
        echo "local zk prover did not become ready after ${prover_ready_timeout_secs}s; log: $prover_log" >&2
        exit 1
      }
    fi

    if [[ -n "{{game}}" ]]; then
      export BASE_ZK_FORK_GAME_ADDRESS="{{game}}"
    fi
    if [[ -n "{{game_index}}" ]]; then
      export BASE_ZK_FORK_GAME_INDEX="{{game_index}}"
    fi
    if [[ -n "{{invalid_index}}" ]]; then
      export BASE_ZK_FORK_INVALID_INDEX="{{invalid_index}}"
    fi

    cargo test --package base-challenger --test zk_fork_dispute -- --ignored --nocapture
