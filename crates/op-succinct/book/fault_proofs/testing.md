# Testing Guide

This guide explains how to run and understand the test suite for the OP Succinct fault dispute game system.

## Overview

The fault-proof crate includes comprehensive end-to-end tests that run actual proposer and challenger binaries against a forked Ethereum network. These tests validate the complete lifecycle of dispute games including creation, challenges, resolution, and bond claims.

## Prerequisites

Before running the tests, ensure you have:

1. **Rust toolchain installed**
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **Nightly Foundry installed** (required for contract bindings)
   ```bash
   curl -L https://foundry.paradigm.xyz | bash
   foundryup --install nightly-d592b3e0f142d694c3be539702704a4a73238773
   ```

   As of July 11, 2025, you need to use Forge nightly for binding generation.

3. **Environment variables configured**
   ```bash
   export L1_RPC=<YOUR_L1_RPC>
   export L1_BEACON_RPC=<YOUR_L1_BEACON_RPC>
   export L2_RPC=<YOUR_L2_RPC>
   export L2_NODE_RPC=<YOUR_L2_NODE_RPC>
   ```

## Available Tests

### End-to-End Tests (`fault_proof/tests/e2e.rs`)

The test suite includes two comprehensive end-to-end tests that validate the complete fault dispute game lifecycle:

#### 1. Honest Proposer Full Lifecycle
`test_honest_proposer()`: Tests the complete lifecycle when proposer creates valid games:
- **Phase 1: Game Creation**: Proposer creates games naturally based on L2 state
- **Phase 2: Challenge Period**: Time warp to near end of challenge duration (no challenges submitted)
- **Phase 3: Resolution**: Games automatically resolve in proposer's favor after challenge period
- **Phase 4: Bond Claims**: Proposer claims bonds after airgap period (7 days warped to seconds)

#### 2. Honest Challenger Full Lifecycle
`test_honest_challenger()`: Tests challenger winning against invalid games:
- **Phase 1: Create Invalid Games**: Test manually creates games with invalid output roots
- **Phase 2: Challenge Period**: Challenger automatically detects and challenges invalid games
- **Phase 3: Resolution**: Time warp past prove deadline, games resolve in challenger's favor
- **Phase 4: Bond Claims**: Challenger claims bonds from all defeated games after airgap period

## Running the Tests

### Basic Test Execution
```bash
# Run all end-to-end tests
cargo test --release --test e2e
```