_sccache := `command -v sccache 2>/dev/null || true`
# Cache compiled artifacts with sccache when it is installed, otherwise fall
# back to the plain compiler.
export RUSTC_WRAPPER := if _sccache != "" { "sccache" } else { "" }
# sccache cannot cache incrementally-compiled crates, so disable incremental
# compilation only when sccache is active.
export CARGO_INCREMENTAL := if _sccache != "" { "0" } else { "1" }

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
# Standalone user-funded prover stack (user RPCs + Succinct Network key)
mod prover 'etc/just/prover.just'
# Local Nitro proof stack for the single-Anvil L1 devnet
mod anvil-nitro-local 'etc/just/anvil-nitro-local.just'
# Prover-service JSON-RPC request helpers
mod zk-prover 'etc/just/zk-prover.just'
# Challenge / dispute helpers
mod challenge 'etc/just/challenge.just'

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

    if ! command -v sccache >/dev/null 2>&1; then
        echo "Installing sccache..."
        if command -v brew >/dev/null 2>&1; then
            brew install sccache
        elif command -v cargo-binstall >/dev/null 2>&1; then
            cargo binstall --no-confirm sccache
        else
            cargo install sccache --locked
        fi
    fi

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

# Apply etc/upstream-pins/reth.toml to every git-based reth-* workspace dep.
# Workflow: etc/upstream-pins/README.md
pin-reth:
    python3 etc/scripts/local/pin-reth.py apply

# Verify Cargo.toml and Cargo.lock match etc/upstream-pins/reth.toml
check-reth-pin:
    python3 etc/scripts/local/pin-reth.py check

# Run unit tests for the Reth pin and release helpers
pin-reth-test:
    python3 etc/scripts/local/pin-reth.py test

# Squash GitHub PRs onto an official tag, publish the fork tag, and pin it
reth-prepare-release *args:
    python3 etc/scripts/local/pin-reth.py prepare {{ args }}

# Fixes any formatting issues
format-fix:
    BASE_SUCCINCT_ELF_STUB=1 cargo fix --allow-dirty --allow-staged --workspace
    cargo +nightly fmt --all

# Fixes any clippy issues
clippy-fix:
    BASE_SUCCINCT_ELF_STUB=1 cargo clippy --workspace --all-features --all-targets --fix --allow-dirty --allow-staged

# Cleans the workspace
clean:
    cargo clean

# Watches tests
watch-test: build::contracts
    cargo watch -x test

# Watches checks
watch-check:
    cargo watch -x "fmt --all -- --check" -x "clippy --all-features --all-targets -- -D warnings" -x test

# Runs all benchmarks (excludes b20_zk_proving, which requires a live local L2/rollup/prover-service stack)
benches:
    @just bench-flashblocks-pending-state
    @just bench-flashblocks-sender-recovery
    @just bench-proof-mpt
    @just bench-protocol
    @just bench-consensus-derive
    @just bench-precompiles
    @just bench-node-runner
    @just bench-execution-trie-witness-reads
    @just bench-execution-trie-deep-history-reads
    @just bench-builder-core
    @just bench-builder-publish
    @just bench-txpool-validity

# Runs flashblocks pending state benchmarks
bench-flashblocks-pending-state:
    cargo bench -p base-flashblocks-node --bench pending_state

# Runs flashblocks sender recovery benchmarks
bench-flashblocks-sender-recovery:
    cargo bench -p base-flashblocks-node --bench sender_recovery

# Runs MPT trie node benchmarks
bench-proof-mpt:
    cargo bench -p base-proof-mpt --bench trie_node

# Runs consensus protocol batch transaction benchmarks
bench-protocol:
    cargo bench -p base-protocol --bench batch_transaction

# Runs consensus derive batch queue benchmarks
bench-consensus-derive:
    cargo bench -p base-consensus-derive --bench batch_queue --features test-utils

# Runs precompile benchmarks
bench-precompiles:
    cargo bench -p base-common-precompiles --bench base_precompiles --features test-utils

# Runs node runner forkchoice update benchmarks
bench-node-runner:
    cargo bench -p base-node-runner --bench fcu_unsafe

# Runs execution trie witness read benchmarks
bench-execution-trie-witness-reads:
    cargo bench -p base-execution-trie --bench witness_reads

# Runs execution trie deep history read benchmarks
bench-execution-trie-deep-history-reads:
    cargo bench -p base-execution-trie --bench deep_history_reads

# Runs builder core state root benchmarks
bench-builder-core:
    cargo bench -p base-builder-core --bench state_root

# Runs builder publish benchmarks
bench-builder-publish:
    cargo bench -p base-builder-publish --bench publisher

# Runs txpool validity-predicate benchmarks
bench-txpool-validity:
    cargo bench -p base-execution-txpool --bench validity

# Runs txpool same-nonce replacement-lookup benchmark (scan vs indexed)
bench-txpool-admission:
    cargo bench -p base-execution-txpool --bench admission

# Runs the B-20 ZK proving system benchmark (requires a live local L2/rollup/prover-service stack)
bench-b20-zk-proving:
    cargo bench -p base-system-tests --bench b20_zk_proving

# Run basectl TUI dashboard
basectl:
    cargo run -p basectl --release -- monitor
