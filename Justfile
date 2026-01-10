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
    cargo fix --allow-dirty --allow-staged
    cargo +nightly fmt --all

# Checks clippy
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
    cd crates/client/test-utils/contracts && forge build

# Cleans the workspace
clean:
    cargo clean

# Checks if there are any unused dependencies
check-udeps: build-contracts
  @command -v cargo-udeps >/dev/null 2>&1 || cargo install cargo-udeps
  cargo +nightly udeps --workspace --all-features --all-targets

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
