# Bundler Spec Tests (EIP-4337)

End-to-end tests using the official [eth-infinitism/bundler-spec-tests](https://github.com/eth-infinitism/bundler-spec-tests) suite.

## Overview

This directory contains scripts to run the official EIP-4337 bundler compatibility tests against our node implementation. We test against both v0.6 and v0.7 specifications.

## Prerequisites

- **Python 3.8+**: Required for bundler-spec-tests
- **PDM**: Python package manager (`pip install pdm`)
- **Node.js & Yarn**: Required for contract deployment
- **Rust**: For building the node

## Quick Start

### One-Command Setup & Run

```bash
# Run all e2e tests (sets up dependencies, builds node, runs tests)
just e2e-test

# Or run tests for specific versions
just e2e-test-v06
just e2e-test-v07
```

### Manual Setup

```bash
# 1. Clone bundler-spec-tests (one-time setup)
./crates/account-abstraction/e2e/bundler-spec-tests/setup.sh

# 2. Build the node
cargo build --release

# 3. Run tests for v0.6
./crates/account-abstraction/e2e/bundler-spec-tests/run-tests.sh v0.6

# 4. Run tests for v0.7
./crates/account-abstraction/e2e/bundler-spec-tests/run-tests.sh v0.7
```

## Directory Structure

```
crates/account-abstraction/e2e/bundler-spec-tests/
├── README.md              # This file
├── setup.sh               # Clone and setup bundler-spec-tests
├── launcher.sh            # Launcher script for spec tests (start/stop/restart)
├── run-tests.sh           # Main test runner script
├── repos/                 # Cloned test repositories (gitignored)
│   ├── v0.6/              # bundler-spec-tests @ releases/v0.6 branch
│   └── v0.7/              # bundler-spec-tests @ releases/v0.7 branch
└── logs/                  # Test logs (gitignored)
```

## Test Configuration

### EntryPoint Addresses

The tests use canonical EntryPoint addresses:

| Version | EntryPoint Address |
|---------|-------------------|
| v0.6    | `0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789` |
| v0.7    | `0x0000000071727De22E5E9d8BAf0edAc6f37da032` |

### Ports

| Service | Port |
|---------|------|
| Ethereum Node (RPC) | 8545 |
| Bundler RPC | 8545 (same, built-in) |

## Running Individual Tests

You can run specific tests using pytest's `-k` flag:

```bash
# Run a specific test
./crates/account-abstraction/e2e/bundler-spec-tests/run-tests.sh v0.7 -k "test_eth_sendUserOperation"

# Run tests matching a pattern
./crates/account-abstraction/e2e/bundler-spec-tests/run-tests.sh v0.6 -k "gas"
```

## Troubleshooting

### Python/PDM Issues

```bash
# Ensure PDM is installed
pip install --user pdm

# Clear PDM cache if dependencies fail
pdm cache clear
```

### Node Won't Start

Check if another process is using port 8545:
```bash
lsof -i :8545
```

### Contract Deployment Fails

The launcher script deploys contracts automatically. If deployment fails:
1. Check the node is running and accessible at `http://localhost:8545`
2. Ensure the deployer account has funds (dev mode auto-funds)
3. Check logs in `crates/account-abstraction/e2e/bundler-spec-tests/logs/`

## CI Integration

These tests are designed to run in CI. The setup script handles:
- Cloning the correct version of bundler-spec-tests
- Installing Python dependencies
- Building the node if needed

Example CI workflow:
```yaml
- name: Run Bundler Spec Tests
  run: |
    just e2e-test
```

## References

- [EIP-4337 Specification](https://eips.ethereum.org/EIPS/eip-4337)
- [ERC-7769 RPC Specification](https://eips.ethereum.org/EIPS/eip-7769)
- [bundler-spec-tests Repository](https://github.com/eth-infinitism/bundler-spec-tests)
- [Unified ERC-4337 Mempool](https://notes.ethereum.org/@yoav/unified-erc-4337-mempool)
