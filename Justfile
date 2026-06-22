# On macOS, skip risc0-sys kernel compilation for check/clippy commands.
# The kernels require Xcode (Metal) on macOS but are only needed for linking
# (cargo build), not for type-checking (cargo check/clippy). CI builds run
# on Linux where CPU kernels compile without issue.

[private]
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
