# Account Abstraction End-to-End Tests

This directory contains end-to-end test suites for the Account Abstraction (EIP-4337) implementation.

## Test Suites

### Bundler Spec Tests

Official EIP-4337 bundler compatibility tests from [eth-infinitism/bundler-spec-tests](https://github.com/eth-infinitism/bundler-spec-tests).

Tests account abstraction (EIP-4337) bundler functionality against v0.6 and v0.7 specifications.

```bash
# Run all bundler spec tests
just e2e-test

# Run specific version
just e2e-test-v06
just e2e-test-v07
```

See [bundler-spec-tests/README.md](./bundler-spec-tests/README.md) for detailed documentation.

## Quick Reference

| Command | Description |
|---------|-------------|
| `just e2e-setup` | Setup all e2e test dependencies |
| `just e2e-test` | Run all e2e tests (v0.6 and v0.7) |
| `just e2e-test-v06` | Run v0.6 bundler spec tests only |
| `just e2e-test-v07` | Run v0.7 bundler spec tests only |
| `just e2e-test-quick v0.7` | Run tests without rebuilding node |

## Prerequisites

- Python 3.8+ with PDM (`pip install pdm`)
- Node.js and Yarn (for contract deployment)
- Rust toolchain

## Adding New Test Suites

To add a new e2e test suite:

1. Create a new directory under `crates/account-abstraction/e2e/`
2. Add setup and run scripts
3. Update the root `justfile` with appropriate commands
4. Document in this README
