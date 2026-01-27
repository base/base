set positional-arguments := true

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

# Runs tests across workspace with all features enabled
test: build-contracts
    @command -v cargo-nextest >/dev/null 2>&1 || cargo install cargo-nextest
    RUSTFLAGS="-D warnings" cargo nextest run --workspace --all-features

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

# Fixes any clippy issues
clippy-fix:
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged

# Builds the workspace with release
build:
    cargo build --workspace --release

# Builds all targets in debug mode
build-all-targets: build-contracts
    cargo build --workspace --all-targets

# Builds the workspace with maxperf
build-maxperf:
    cargo build --workspace --profile maxperf --features jemalloc

# Builds the base node binary
build-node:
    cargo build --bin base-reth-node

# Build the contracts used for tests
build-contracts:
    cd crates/shared/primitives/contracts && forge build

# Cleans the workspace
clean:
    cargo clean

# Checks if there are any unused dependencies
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

# Builds tester binary (requires testing feature)
build-tester:
    cargo build -p op-rbuilder --bin tester --features "testing"

# Stops devnet, deletes data, and starts fresh
devnet: devnet-down
    docker compose up -d

# Stops devnet and deletes all data
devnet-down:
    -docker compose down
    rm -rf .devnet

devnet-checks rpc="http://localhost:8545" pk="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80" to="0x70997970C51812dc3A010C7d01b50e0d17dc79C8":
    @echo "Block: $(cast block-number --rpc-url {{rpc}})"
    @echo "Sending ETH tx..." && cast send --private-key {{pk}} --rpc-url {{rpc}} {{to}} --value 0.001ether --json | jq -r '"ETH tx: \(.transactionHash) block=\(.blockNumber) status=\(.status)"'
    @echo "Sending blob tx..." && echo "blob" | cast send --private-key {{pk}} --rpc-url {{rpc}} --blob --path /dev/stdin {{to}} --json | jq -r '"Blob tx: \(.transactionHash) block=\(.blockNumber) status=\(.status) blobGas=\(.blobGasUsed)"'
