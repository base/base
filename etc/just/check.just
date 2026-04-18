_skip_kernels := if os() == "macos" { "RISC0_SKIP_BUILD_KERNELS=1" } else { "" }

# Show check commands
default:
    @just --justfile {{source_file()}} --list --list-prefix '  check::'

# Runs formatting, clippy, udeps, and deny checks
all: format clippy udeps deny

# Checks formatting
format:
    cargo +nightly fmt --all -- --check

# Checks clippy
clippy: _build-contracts
    {{_skip_kernels}} cargo clippy --workspace --all-targets -- -D warnings

# Checks clippy with ci profile
clippy-ci: _build-contracts
    {{_skip_kernels}} cargo clippy --locked --workspace --all-targets --profile ci -- -D warnings

# Checks unused dependencies
udeps: _build-contracts
    @command -v cargo-udeps >/dev/null 2>&1 || cargo install cargo-udeps
    {{_skip_kernels}} cargo +nightly udeps --locked --workspace --all-features --all-targets

# Runs cargo deny to check dependencies
deny:
    @command -v cargo-deny >/dev/null 2>&1 || cargo install cargo-deny
    (cd {{justfile_directory()}} && cargo deny check bans --hide-inclusion-graph)

# Checks no_std crates compile without std
no-std:
    {{justfile_directory()}}/etc/scripts/ci/check-no-std.sh

# Checks proof crates compile for bare-metal FPVM
no-std-proof:
    {{justfile_directory()}}/etc/scripts/ci/check-no-std-proof.sh

# Checks crate dependency boundary rules
crate-deps:
    {{justfile_directory()}}/etc/scripts/ci/check-crate-deps.sh

[private]
_build-contracts:
    cd {{justfile_directory()}}/crates/utilities/test-utils/contracts && forge soldeer install && forge build
