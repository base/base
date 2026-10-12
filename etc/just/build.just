# Show build commands
default:
    @just --justfile {{source_file()}} --list --list-prefix '  build::'

# Builds the workspace with release
release:
    cargo build --workspace --release

# Builds all targets in debug mode
all-targets: contracts elfs
    cargo build --workspace --all-targets

# Builds all targets with ci profile
ci: contracts
    cargo build --locked --workspace --all-targets --profile ci

elfs:
    just succinct build-elfs

# Builds the base node binary
node:
    cargo build --bin base-reth-node

# Builds the base-bench binary in release mode
base-bench:
    cargo build --release -p base-system-tests --bin base-bench

# Builds the test contracts
contracts:
    cd {{justfile_directory()}}/crates/utilities/test-utils/contracts && forge soldeer install && forge build
