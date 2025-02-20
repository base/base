# End-to-End Tests

This guide explains how to run and understand the end-to-end tests for the OP Succinct fault dispute game system. All tests are in the [fault_proof/tests/e2e.rs](../../fault_proof/tests/e2e.rs) file.

## Prerequisites

Before running the tests, ensure you have:
1. Rust toolchain installed
2. Environment variables properly configured
3. Access to L1 and L2 test networks
4. Sufficient test ETH for transactions

## Configuration

See [Proposer Configuration](./proposer.md#configuration) and [Challenger Configuration](./challenger.md#configuration) for more information on how to configure the proposer and challenger properly.

## Available Tests

### 1. Proposer Wins Scenario
`test_e2e_proposer_wins()`: Tests the happy path where:
- The honest proposer creates valid games
- No challenges are submitted
- Games are resolved successfully in favor of the proposer after timeout

### 2. Challenger Wins Scenario
`test_e2e_challenger_wins()`: Tests the scenario where:
- The malicious proposer creates invalid games
- The challenger successfully challenges them
- Games resolve in favor of the challenger after timeout

## Running the Tests

To run an e2e test:
```bash
cargo test --test e2e <TEST_NAME>
```
