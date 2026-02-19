set positional-arguments := true
set dotenv-filename := "etc/docker/devnet-env"

alias t := test
alias f := fix
alias b := build
alias be := benches
alias c := clean
alias h := hack
alias u := check-udeps
alias wt := watch-test
alias wc := watch-check

# Default to display help menu
default:
    @just --list

# Runs all ci checks
ci: fix check lychee zepter

# Performs lychee checks, installing the lychee command if necessary
lychee:
    @command -v lychee >/dev/null 2>&1 || cargo install lychee
    lychee --config ./lychee.toml .

# Checks formatting, udeps, clippy, and tests
check: check-format check-udeps check-clippy test check-deny

# Runs cargo deny to check dependencies
check-deny:
    @command -v cargo-deny >/dev/null 2>&1 || cargo install cargo-deny
    cargo deny check bans --hide-inclusion-graph

# Fixes formatting and clippy issues
fix: format-fix clippy-fix zepter-fix

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
    @command -v cargo-nextest >/dev/null 2>&1 || cargo install cargo-nextest

# Runs tests across workspace with all features enabled (excludes devnet)
test: install-nextest build-contracts
    RUSTFLAGS="-D warnings" cargo nextest run --workspace --all-features --exclude devnet

# Runs tests with ci profile for minimal disk usage
test-ci: install-nextest build-contracts
    RUSTFLAGS="-D warnings" cargo nextest run --workspace --all-features --exclude devnet --cargo-profile ci

# Runs devnet tests (requires Docker)
devnet-tests: install-nextest build-contracts
    cargo nextest run -p devnet

# Runs devnet tests with ci profile for minimal disk usage
devnet-tests-ci: install-nextest build-contracts
    cargo nextest run -p devnet --cargo-profile ci

# Pre-pulls Docker images needed for system tests
system-tests-pull-images:
    docker build -t devnet-setup:local -f etc/docker/Dockerfile.devnet .
    docker pull ghcr.io/paradigmxyz/reth:v1.10.2
    docker pull sigp/lighthouse:v8.0.1
    docker pull us-docker.pkg.dev/oplabs-tools-artifacts/images/op-node:v1.16.5
    docker pull us-docker.pkg.dev/oplabs-tools-artifacts/images/op-batcher:v1.16.3

# Runs cargo hack against the workspace
hack:
    cargo hack check --feature-powerset --no-dev-deps

# Checks formatting
check-format:
    cargo +nightly fmt --all -- --check

# Fixes any formatting issues
format-fix:
    cargo fix --allow-dirty --allow-staged --workspace
    cargo +nightly fmt --all

# Checks clippy
check-clippy: build-contracts
    cargo clippy --workspace --all-targets -- -D warnings

# Checks clippy with ci profile for minimal disk usage
check-clippy-ci: build-contracts
    cargo clippy --workspace --all-targets --profile ci -- -D warnings

# Fixes any clippy issues
clippy-fix:
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged

# Builds the workspace with release
build:
    cargo build --workspace --release

# Builds all targets in debug mode
build-all-targets: build-contracts
    cargo build --workspace --all-targets

# Builds all targets with ci profile (minimal disk usage for CI)
build-ci: build-contracts
    cargo build --workspace --all-targets --profile ci

# Builds the workspace with maxperf
build-maxperf:
    cargo build --workspace --profile maxperf --features jemalloc

# Builds the base node binary
build-node:
    cargo build --bin base-reth-node

# Build the contracts used for tests
build-contracts:
    cd crates/shared/primitives/contracts && forge soldeer install && forge build

# Cleans the workspace
clean:
    cargo clean

# Checks if there are any unused dependencies
check-udeps: build-contracts
    @command -v cargo-udeps >/dev/null 2>&1 || cargo install cargo-udeps
    cargo +nightly udeps --workspace --all-features --all-targets

# Checks crate dependency boundary rules
check-crate-deps:
    ./etc/scripts/ci/check-crate-deps.sh

# Watches tests
watch-test: build-contracts
    cargo watch -x test

# Watches checks
watch-check:
    cargo watch -x "fmt --all -- --check" -x "clippy --all-targets -- -D warnings" -x test

# Runs all benchmarks
benches:
    @just bench-flashblocks

# Runs flashblocks pending state benchmarks
bench-flashblocks:
    cargo bench -p base-flashblocks --bench pending_state

# Stops devnet, deletes data, and starts fresh
devnet: devnet-down
    docker compose --env-file etc/docker/devnet-env -f etc/docker/docker-compose.yml up -d --build --scale contender=0

# Stops devnet, deletes data, and starts fresh with profiling (Pyroscope + optimized builds)
devnet-profiling: devnet-down
    CARGO_PROFILE=profiling docker compose --env-file etc/docker/devnet-env -f etc/docker/docker-compose.yml --profile profiling up -d --build --scale contender=0

# Stops devnet and deletes all data
devnet-down:
    -docker compose --env-file etc/docker/devnet-env -f etc/docker/docker-compose.yml --profile profiling down
    rm -rf .devnet

# Shows devnet block numbers and sync status
devnet-status:
    ./etc/scripts/devnet/status.sh

# Shows funded test accounts with live balances and nonces
devnet-accounts:
    ./etc/scripts/devnet/accounts.sh

# Sends test transactions to L1 and L2
devnet-smoke:
    ./etc/scripts/devnet/smoke.sh

# Runs full devnet checks (status + smoke tests)
devnet-checks: devnet-status devnet-smoke

# Starts the contender load generator
devnet-load:
    docker compose -f etc/docker/docker-compose.yml up -d --no-deps contender

# Stops the contender load generator
devnet-load-down:
    docker compose -f etc/docker/docker-compose.yml down contender

# Stream FB's from the builder via websocket
devnet-flashblocks:
    @command -v flashblocks-websocket-client >/dev/null 2>&1 || go install github.com/danyalprout/flashblocks-websocket-client@latest
    flashblocks-websocket-client ws://localhost:${L2_BUILDER_FLASHBLOCKS_PORT}

# Stream logs from devnet containers (optionally specify container names)
devnet-logs *containers:
    docker compose --env-file etc/docker/devnet-env -f etc/docker/docker-compose.yml logs -f {{ containers }}

# Run basectl with specified config (mainnet, sepolia, devnet, or path)
basectl config="mainnet":
    cargo run -p basectl --release -- -c {{config}}
