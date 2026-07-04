# On macOS, skip risc0-sys kernel compilation for check/clippy commands.
# The kernels require Xcode (Metal) on macOS but are only needed for linking
# (cargo build), not for type-checking (cargo check/clippy). CI builds run
# on Linux where CPU kernels compile without issue.

[private]
_skip_kernels := if os() == "macos" { "RISC0_SKIP_BUILD_KERNELS=1" } else { "" }

set positional-arguments := true
set dotenv-load

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
# ZK prover gRPC request helpers
mod zk-prover 'etc/just/zk-prover.just'

alias t := test
alias f := fix
alias be := benches
alias c := clean
alias h := hack
alias wt := watch-test
alias wc := watch-check
alias ldc := load-test-continuous

# Default to display help menu
default:
    @just --list

# Load test a network in continuous mode (Ctrl-C to stop)
load-test-continuous network='devnet':
    just load-test continuous {{ network }}

# One-time project setup: installs tooling and builds test contracts
setup:
    #!/usr/bin/env bash
    set -euo pipefail

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

# Runs tests across workspace with all features enabled (excludes system tests)
test: install-nextest build::contracts build::elfs
    cargo nextest run --workspace --all-features --exclude base-system-tests --no-fail-fast

# Runs tests only for crates affected by changes vs main (excludes system tests)
test-affected base="main": install-nextest build::contracts build::elfs
    #!/usr/bin/env bash
    set -euo pipefail
    pkg_args_output="$(python3 etc/scripts/local/affected-crates.py {{ base }} --exclude base-system-tests --cargo-args)"
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
test-ci: install-nextest build::contracts
    cargo nextest run -P ci --locked --workspace --all-features --exclude base-system-tests --cargo-profile ci

# Runs tests only for affected crates with ci profile (for PRs)
test-affected-ci base="main": install-nextest build::contracts
    #!/usr/bin/env bash
    set -euo pipefail
    pkg_args_output="$(python3 etc/scripts/local/affected-crates.py {{ base }} --exclude base-system-tests --cargo-args)"
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
    {{ _skip_kernels }} BASE_SUCCINCT_ELF_STUB=1 cargo fix --allow-dirty --allow-staged --workspace
    cargo +nightly fmt --all

# Fixes any clippy issues
clippy-fix:
    {{ _skip_kernels }} BASE_SUCCINCT_ELF_STUB=1 cargo clippy --workspace --all-features --all-targets --fix --allow-dirty --allow-staged

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
    cargo run -p basectl --release -- monitor

# Run local Nitro+TDX proof workers, prover-service, and proposer against Base Sepolia.
sepolia-tdx-dev-offchain:
    #!/usr/bin/env bash
    set -euo pipefail

    for bin in cargo cast docker jq python3; do
        command -v "$bin" >/dev/null 2>&1 || {
            echo "Missing required command: $bin" >&2
            exit 1
        }
    done

    l1_rpc="${L1_RPC_URL:-https://ethereum-full-sepolia-k8s-dev.cbhq.net}"
    l2_rpc="${L2_RPC_URL:-https://base-sepolia-reth-proofs-k8s-donotuse.cbhq.net:8545}"
    rollup_rpc="${ROLLUP_RPC_URL:-${L2_OUTPUT_ROOT_RPC_URL:-https://base-sepolia-reth-internal-rpc-donotuse.cbhq.net:7545}}"
    l1_beacon="${L1_BEACON_URL:-https://ethereum-full-sepolia-k8s-dev.cbhq.net:5052}"
    l2_chain_id="${TDX_SEPOLIA_L2_CHAIN_ID:-84532}"
    postgres_port="${TDX_SEPOLIA_POSTGRES_PORT:-5432}"
    requester_rpc="${TDX_SEPOLIA_PROVER_REQUESTER_RPC:-127.0.0.1:9000}"
    worker_rpc="${TDX_SEPOLIA_PROVER_WORKER_RPC:-127.0.0.1:9001}"
    nitro_signer_rpc="${TDX_SEPOLIA_NITRO_SIGNER_RPC:-127.0.0.1:8000}"
    tdx_signer_rpc="${TDX_SEPOLIA_TDX_SIGNER_RPC:-127.0.0.1:8010}"
    proposer_private_key="${BASE_PROPOSER_PRIVATE_KEY:?BASE_PROPOSER_PRIVATE_KEY must be set in .env or the environment}"
    forge_account="${TDX_SEPOLIA_FORGE_ACCOUNT:-testnet-admin}"
    contracts_dir="$(cd ../contracts && pwd)"
    deployments="${TDX_SEPOLIA_DEPLOYMENTS:-$contracts_dir/deployments/11155111-dev-with-tdx.json}"
    deploy_config="${TDX_SEPOLIA_DEPLOY_CONFIG:-$contracts_dir/deploy-config/zeronet-tdx.json}"
    pg_container="${TDX_SEPOLIA_POSTGRES_CONTAINER:-base-prover-service-tdx-sepolia}"
    pg_data_dir="${TDX_SEPOLIA_POSTGRES_DATA_DIR:-$PWD/.tdx-sepolia/postgres}"
    pg_password="${TDX_SEPOLIA_POSTGRES_PASSWORD:-postgres}"

    registry="$(jq -er '.TEEProverRegistry' "$deployments")"
    anchor_state_registry="$(jq -er '.AnchorStateRegistry' "$deployments")"
    dispute_game_factory="$(jq -er '.DisputeGameFactory' "$deployments")"
    game_type="$(jq -r '.multiproofGameType // 621' "$deploy_config")"
    nitro_image_hash="$(jq -er '.teeNitroImageHash' "$deploy_config")"
    tdx_image_hash="$(jq -er '.teeTdxImageHash' "$deploy_config")"

    wait_rpc() {
        local url="$1" name="$2"
        shift 2
        for _ in {1..120}; do
            "$@" --rpc-url "$url" >/dev/null 2>&1 && return 0
            sleep 1
        done
        echo "Timed out waiting for $name at $url" >&2
        exit 1
    }

    signer_from_rpc() {
        local public_key_body public_key_hash
        public_key_body="$(cast rpc enclave_signerPublicKey --rpc-url "$1" \
            | python3 -c "import json, sys; data = json.load(sys.stdin)[0]; print('0x' + bytes(data[1:]).hex())")"
        public_key_hash="$(cast keccak "$public_key_body")"
        echo "0x${public_key_hash: -40}"
    }

    echo "Building local offchain binaries"
    cargo build -p base-prover-service-bin -p base-prover-tdx -p base-proposer-bin
    cargo build -p base-prover-nitro-host --features local,worker
    export RUST_LOG="${RUST_LOG:-info}"

    pids=()
    cleanup() {
        for pid in "${pids[@]:-}"; do
            kill "$pid" 2>/dev/null || true
        done
        wait 2>/dev/null || true
    }
    trap cleanup EXIT INT TERM

    create_pg_container() {
        docker run -d \
            --name "$pg_container" \
            -e POSTGRES_USER=postgres \
            -e POSTGRES_PASSWORD="$pg_password" \
            -e POSTGRES_DB=proverdb \
            -p "127.0.0.1:$postgres_port:5432" \
            -v "$pg_data_dir:/var/lib/postgresql/data" \
            -v "$PWD/crates/proof/prover-service/db/migrations:/docker-entrypoint-initdb.d:ro" \
            postgres:17-alpine >/dev/null
    }

    mkdir -p "$pg_data_dir"
    # ponytail: initdb migrations run on first DB creation; delete the data dir to replay them.
    docker rm -f "$pg_container" >/dev/null 2>&1 || true
    create_pg_container
    until docker exec "$pg_container" pg_isready -U postgres -d proverdb >/dev/null 2>&1; do
        sleep 1
    done

    echo "Starting prover-service"
    POSTGRES_HOST=127.0.0.1 \
    POSTGRES_PORT="$postgres_port" \
    POSTGRES_DB=proverdb \
    POSTGRES_USER=postgres \
    POSTGRES_PASSWORD="$pg_password" \
    POSTGRES_SSLMODE=disable \
        target/debug/base-prover-service \
        --rpc-listen-addr "$requester_rpc" \
        --worker-rpc-listen-addr "$worker_rpc" &
    pids+=("$!")
    wait_rpc "http://$requester_rpc" prover-service-requester \
        cast rpc prover_listProofs '{"offset":0,"limit":1}'
    wait_rpc "http://$worker_rpc" prover-service-worker \
        cast rpc prover_getProofSession '{"session_id":"__ready__","session_type":"stark"}'

    echo "Starting Nitro worker"
    target/debug/base-prover-nitro-host local \
        --l1-eth-url "$l1_rpc" \
        --l2-eth-url "$l2_rpc" \
        --l1-beacon-url "$l1_beacon" \
        --l2-chain-id "$l2_chain_id" \
        --listen-addr "$nitro_signer_rpc" \
        --prover-service-endpoint "http://$worker_rpc" \
        --enable-experimental-witness-endpoint &
    pids+=("$!")
    wait_rpc "http://$nitro_signer_rpc" nitro-signer-rpc cast rpc enclave_signerPublicKey

    echo "Starting TDX worker"
    target/debug/base-prover-tdx local \
        --l1-eth-url "$l1_rpc" \
        --l2-eth-url "$l2_rpc" \
        --l1-beacon-url "$l1_beacon" \
        --l2-chain-id "$l2_chain_id" \
        --listen-addr "$tdx_signer_rpc" \
        --prover-service-endpoint "http://$worker_rpc" \
        --enable-experimental-witness-endpoint &
    pids+=("$!")
    wait_rpc "http://$tdx_signer_rpc" tdx-signer-rpc cast rpc enclave_signerPublicKey

    nitro_signer="$(signer_from_rpc "http://$nitro_signer_rpc")"
    tdx_signer="$(signer_from_rpc "http://$tdx_signer_rpc")"

    echo "Registering local TEE signers in $registry"
    owner="$(cast wallet address --account "$forge_account")"
    cast send "$registry" "addDevSigner(address,bytes32,uint8)" "$nitro_signer" "$nitro_image_hash" 1 \
        --rpc-url "$l1_rpc" --account "$forge_account" --from "$owner"
    cast send "$registry" "addDevSigner(address,bytes32,uint8)" "$tdx_signer" "$tdx_image_hash" 2 \
        --rpc-url "$l1_rpc" --account "$forge_account" --from "$owner"

    echo "Starting proposer"
    echo "Proposer address: $(cast wallet address "$proposer_private_key")"
    target/debug/base-proposer \
        --prover-rpc "http://$requester_rpc" \
        --l1-eth-rpc "$l1_rpc" \
        --l2-eth-rpc "$l2_rpc" \
        --rollup-rpc "$rollup_rpc" \
        --anchor-state-registry-addr "$anchor_state_registry" \
        --dispute-game-factory-addr "$dispute_game_factory" \
        --game-type "$game_type" \
        --tee-proof-mode both \
        --allow-non-finalized \
        --private-key "$proposer_private_key" \
        --poll-interval "${TDX_SEPOLIA_PROPOSER_POLL_INTERVAL:-12s}"
