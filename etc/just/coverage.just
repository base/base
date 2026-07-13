# Code coverage collection via cargo-llvm-cov.
#
# Coverage is gathered on the host for three test tiers, each emitting its own
# lcov file so they can be uploaded to Codecov under distinct flags:
#   - unit:   workspace unit + integration tests (excludes system tests)
#   - action: the base-action-harness action tests
#   - system: the Docker-backed system tests (base-system-tests)
#
# The L2 `base` node runs in-process inside the system-test binary (see
# etc/systems/src/l2/stack.rs), so host-side instrumentation captures node
# coverage directly. Only third-party L1 containers (reth/lighthouse) run in
# Docker, and those are not instrumented.

set working-directory := '../..'

# Show coverage commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  cov::'

# Installs cargo-llvm-cov if not present
[private]
_install-llvm-cov:
    @command -v cargo-llvm-cov >/dev/null 2>&1 || cargo install cargo-llvm-cov --locked

# Installs cargo-nextest if not present
[private]
_install-nextest:
    @command -v cargo-nextest >/dev/null 2>&1 || cargo install cargo-nextest --locked

# Builds the test contracts required by the test suites
[private]
_build-contracts:
    cd crates/utilities/test-utils/contracts && forge soldeer install && forge build

# Removes previously collected coverage artifacts
clean:
    cargo llvm-cov clean --workspace
    rm -rf target/coverage

# Collects coverage for workspace unit + integration tests (excludes system tests)
unit *args: _install-llvm-cov _install-nextest _build-contracts
    mkdir -p target/coverage
    cargo llvm-cov nextest --workspace --all-features --exclude base-system-tests --lcov --output-path target/coverage/unit.lcov {{ args }}

# Collects coverage for the action-test harness (base-action-harness)
action *args: _install-llvm-cov _install-nextest _build-contracts
    mkdir -p target/coverage
    cargo llvm-cov nextest -p base-action-harness --lcov --output-path target/coverage/action.lcov {{ args }}

# Collects coverage for the Docker-backed system tests (needs `just devnet pull-images`)
system *args: _install-llvm-cov _install-nextest _build-contracts
    mkdir -p target/coverage
    cargo llvm-cov nextest -p base-system-tests --lcov --output-path target/coverage/system.lcov {{ args }}

# Collects coverage for all three tiers
all: unit action system

# Generates a local HTML report for the workspace (excludes system tests)
html: _install-llvm-cov _install-nextest _build-contracts
    cargo llvm-cov nextest --workspace --all-features --exclude base-system-tests --html
    @echo "Open target/llvm-cov/html/index.html"
