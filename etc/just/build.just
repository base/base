# Show build commands
default:
    @just --justfile {{source_file()}} --list --list-prefix '  build::'

# Builds the workspace with release
release:
    cargo build --workspace --release

# Builds all targets in debug mode
all-targets: contracts
    cargo build --workspace --all-targets

# Builds all targets with ci profile
ci: contracts
    cargo build --locked --workspace --all-targets --profile ci

# Builds the workspace with maxperf
maxperf:
    cargo build --workspace --profile maxperf --features jemalloc

# Builds the base node binary
node:
    cargo build --bin base-reth-node

# Builds the test contracts
contracts:
    cd {{justfile_directory()}}/crates/utilities/test-utils/contracts && forge soldeer install && forge build
