set positional-arguments
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

# Runs all ci checks.
ci: fix check lychee zepter

# Performs lychee checks, installing the lychee command if necessary
lychee:
  @command -v lychee >/dev/null 2>&1 || cargo install lychee
  lychee --config ./lychee.toml .

# Checks formatting, udeps, clippy, and tests
check: check-format check-udeps check-clippy test

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

# Runs tests across workspace with all features enabled (excludes builder crates)
test: build-contracts
    @command -v cargo-nextest >/dev/null 2>&1 || cargo install cargo-nextest
    RUSTFLAGS="-D warnings" cargo nextest run --workspace --all-features

# Runs cargo hack against the workspace
hack:
  cargo hack check --feature-powerset --no-dev-deps

# Checks formatting (builder crates excluded via rustfmt.toml)
check-format:
    cargo +nightly fmt --all -- --check

# Fixes any formatting issues (builder crates excluded via rustfmt.toml)
format-fix:
    cargo fix --allow-dirty --allow-staged --workspace
    cargo +nightly fmt --all

# Checks clippy (excludes builder crates)
check-clippy:
    cargo clippy --all-targets -- -D warnings

# Fixes any clippy issues
clippy-fix:
    cargo clippy --all-targets --fix --allow-dirty --allow-staged

# Builds the workspace with release
build:
    cargo build --release

# Builds all targets in debug mode
build-all-targets:
    cargo build --all-targets

# Builds the workspace with maxperf
build-maxperf:
    cargo build --profile maxperf --features jemalloc

# Builds the base node binary
build-node:
    cargo build --bin base-reth-node

# Build the contracts used for tests
build-contracts:
    cd crates/shared/primitives/contracts && forge build

# Cleans the workspace
clean:
    cargo clean

# Checks if there are any unused dependencies (excludes builder crates)
check-udeps: build-contracts
  @command -v cargo-udeps >/dev/null 2>&1 || cargo install cargo-udeps
  cargo +nightly udeps --workspace --all-features --all-targets

# Checks that shared crates don't depend on client crates
check-crate-deps:
    ./scripts/check-crate-deps.sh

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

# ============================================
# Builder Crate Targets
# ============================================

# Builds builder crates
build-builder:
    cargo build -p op-rbuilder -p p2p -p tdx-quote-provider

# Builds op-rbuilder binary
build-op-rbuilder:
    cargo build -p op-rbuilder --bin op-rbuilder

# Builds tester binary (requires testing feature)
build-tester:
    cargo build -p op-rbuilder --bin tester --features "testing"

# Builds tdx-quote-provider binary
build-tdx-quote-provider:
    cargo build -p tdx-quote-provider --bin tdx-quote-provider

# Runs tests for builder crates (with OTEL env vars disabled)
test-builder:
    OTEL_EXPORTER_OTLP_ENDPOINT="" OTEL_EXPORTER_OTLP_HEADERS="" OTEL_SDK_DISABLED="true" \
    cargo test -p op-rbuilder -p p2p -p tdx-quote-provider --verbose

# Runs clippy on builder crates (using nightly, matching op-rbuilder's original)
check-clippy-builder:
    cargo +nightly clippy -p op-rbuilder -p p2p -p tdx-quote-provider --all-features -- -D warnings

# Fixes formatting for builder crates
format-builder:
    cargo +nightly fmt -p op-rbuilder -p p2p -p tdx-quote-provider

# Full builder CI check
ci-builder: build-builder test-builder check-clippy-builder

# Builds builder release binary
build-builder-release:
    cargo build --release -p op-rbuilder --bin op-rbuilder

# Builds builder with maxperf profile
build-builder-maxperf:
    cargo build --profile maxperf -p op-rbuilder --bin op-rbuilder --features jemalloc

# ============================================
# Node Run Targets
# ============================================

# Run the node with Account Abstraction enabled (dev mode)
run-node-aa:
    ./crates/client/account-abstraction/start_dev_node.sh

# Run the node with all extensions enabled (dev mode)
run-node-dev AA_SEND_URL="http://localhost:8080":
    cargo build --release -p base-reth-node
    RUST_LOG=info ./target/release/base-reth-node node \
        --datadir /tmp/reth-dev \
        --chain dev \
        --http \
        --http.api eth,net,web3,debug,trace,txpool,rpc,admin \
        --http.addr 0.0.0.0 \
        --http.port 8545 \
        --dev \
        --enable-metering \
        --account-abstraction.enabled \
        --account-abstraction.send-url "{{AA_SEND_URL}}" \
        --account-abstraction.indexer

# Run the node with only AA enabled (no other extensions)
run-node-aa-only AA_SEND_URL="http://localhost:8080":
    cargo build --release -p base-reth-node
    RUST_LOG=info ./target/release/base-reth-node node \
        --datadir /tmp/reth-aa \
        --chain dev \
        --http \
        --http.api eth,net,web3,debug,trace,txpool,rpc,admin \
        --http.addr 0.0.0.0 \
        --http.port 8545 \
        --dev \
        --account-abstraction.enabled \
        --account-abstraction.send-url "{{AA_SEND_URL}}" \
        --account-abstraction.indexer \
        --account-abstraction.debug
