# Testing Guide

This guide explains how to run and understand the test suite for the OP Succinct fault dispute game system. Tests are located in:
- End-to-end tests: fault_proof/tests/e2e.rs
- Integration tests: fault_proof/tests/integration.rs

## Prerequisites

Before running the tests, ensure you have:
1. Rust toolchain installed
2. Environment variables properly configured
3. Access to L1 and L2 test networks
4. Sufficient test ETH for transactions

## Configuration

See [Proposer Configuration](./proposer.md#configuration) and [Challenger Configuration](./challenger.md#configuration) for more information on how to configure the proposer and challenger properly.

## Available Tests

### Integration Tests

#### 1. Proposer Defense Scenario
`test_proposer_defends_successfully()`: Tests the scenario where:
- The proposer creates a valid game
- A malicious challenger challenges it
- The proposer successfully defends with a valid proof

### End-to-End Tests

#### 1. Proposer Wins Scenario
`test_e2e_proposer_wins()`: Tests the happy path where:
- The honest proposer creates valid games
- No challenges are submitted
- Games are resolved successfully in favor of the proposer after timeout

#### 2. Challenger Wins Scenario
`test_e2e_challenger_wins()`: Tests the scenario where:
- The malicious proposer creates invalid games
- The challenger successfully challenges them
- Games resolve in favor of the challenger after timeout

## Running the Tests

To run a specific test:
```bash
# For e2e tests
cargo test --test e2e <TEST_NAME>

# For integration tests
cargo test --test integration <TEST_NAME>
```

For example:
```bash
# Run the proposer defense test
cargo test --test integration test_proposer_defends_successfully

# Run the proposer wins e2e test
cargo test --test e2e test_e2e_proposer_wins
```
