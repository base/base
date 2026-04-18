# On macOS, skip risc0-sys kernel compilation for check/clippy commands.
# The kernels require Xcode (Metal) on macOS but are only needed for linking
# (cargo build), not for type-checking (cargo check/clippy). CI builds run
# on Linux where CPU kernels compile without issue.
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

alias t := test
alias f := fix
alias be := benches
alias c := clean
alias h := hack
alias wt := watch-test
alias wc := watch-check
alias ldc := load-test-devnet-continuous

# Default to display help menu
default:
    @just --list

# Load test devnet in continuous mode (Ctrl-C to stop)
load-test-devnet-continuous:
    just load-test devnet-continuous

# Runs the specs docs locally
specs:
    cd docs/specs && bun ci && bun dev

# One-time project setup: installs tooling and builds test contracts
setup:
    #!/usr/bin/env bash
    set -euo pipefail

    OS="$(uname -s)"
    ARCH="$(uname -m)"

    # ── Install fast linker ──
    if [[ "$OS" == "Darwin" ]]; then
        if ! brew list lld &>/dev/null; then
            echo "Installing lld linker for faster builds..."
            brew install lld
        fi
        # Verify lld is reachable at the path .cargo/config.toml expects
        if [[ "$ARCH" == "arm64" ]]; then
            LLD="/opt/homebrew/opt/lld/bin/ld64.lld"
        else
            LLD="/usr/local/opt/lld/bin/ld64.lld"
        fi
        if [[ ! -x "$LLD" ]]; then
            echo "ERROR: lld not found at $LLD"
            echo "Try: brew install lld"
            exit 1
        fi
        echo "Found lld at $LLD"
    elif [[ "$OS" == "Linux" ]]; then
        if ! command -v mold &>/dev/null; then
            echo "mold not found. Install it for faster builds:"
            echo "  Ubuntu/Debian: sudo apt-get install -y mold"
            echo "  Fedora:        sudo dnf install mold"
            echo "  Arch:          sudo pacman -S mold"
            exit 1
        fi
        echo "Found mold at $(command -v mold)"
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

# Runs tests across workspace with all features enabled (excludes devnet)
test: install-nextest build::contracts
    cargo nextest run --workspace --all-features --exclude devnet --no-fail-fast

# Runs tests only for crates affected by changes vs main (excludes devnet)
test-affected base="main": install-nextest build::contracts
    #!/usr/bin/env bash
    set -euo pipefail
    affected=$(python3 etc/scripts/local/affected-crates.py {{ base }} --exclude devnet)
    if [ -z "$affected" ]; then
        echo "No affected crates to test."
        exit 0
    fi
    pkg_args=""
    while IFS= read -r crate; do
        pkg_args="$pkg_args -p $crate"
    done <<< "$affected"
    echo "Testing affected crates:$pkg_args"
    cargo nextest run --all-features $pkg_args

# Runs tests with ci profile for minimal disk usage
test-ci: install-nextest build::contracts
    cargo nextest run --locked --workspace --all-features --exclude devnet --cargo-profile ci

# Runs tests only for affected crates with ci profile (for PRs)
test-affected-ci base="main": install-nextest build::contracts
    #!/usr/bin/env bash
    set -euo pipefail
    affected=$(python3 etc/scripts/local/affected-crates.py {{ base }} --exclude devnet)
    if [ -z "$affected" ]; then
        echo "No affected crates to test."
        exit 0
    fi
    pkg_args=""
    while IFS= read -r crate; do
        pkg_args="$pkg_args -p $crate"
    done <<< "$affected"
    echo "Testing affected crates:$pkg_args"
    cargo nextest run --locked --all-features --cargo-profile ci $pkg_args || {
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
    {{_skip_kernels}} cargo fix --allow-dirty --allow-staged --workspace
    cargo +nightly fmt --all

# Fixes any clippy issues
clippy-fix:
    {{_skip_kernels}} cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged

# Cleans the workspace
clean:
    cargo clean

# Watches tests
watch-test: build::contracts
    cargo watch -x test

# Watches checks
watch-check:
    cargo watch -x "fmt --all -- --check" -x "clippy --all-targets -- -D warnings" -x test

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
    cargo run -p basectl --release
