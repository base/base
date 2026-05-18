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
ci: contracts elfs
    cargo build --locked --workspace --all-targets --profile ci

# Builds only affected packages with ci profile
affected-ci base="main": contracts elfs
    #!/usr/bin/env bash
    set -euo pipefail
    pkg_args_output="$(python3 {{justfile_directory()}}/etc/scripts/local/affected-crates.py {{ base }} --cargo-args)"
    pkg_args=()
    while IFS= read -r line; do
        [ -n "$line" ] && pkg_args+=("$line")
    done <<< "$pkg_args_output"
    if [ "${#pkg_args[@]}" -eq 0 ]; then
        echo "No affected crates to build."
        exit 0
    fi
    echo "Building affected crates:${pkg_args[*]}"
    cargo build --locked --all-targets --profile ci "${pkg_args[@]}"

elfs:
    just succinct ensure-elfs

# Builds the workspace with maxperf
maxperf:
    cargo build --workspace --profile maxperf --features jemalloc

# Builds the base node binary
node:
    cargo build --bin base-reth-node

# Builds the test contracts
contracts:
    cd {{justfile_directory()}}/crates/utilities/test-utils/contracts && forge soldeer install && forge build
