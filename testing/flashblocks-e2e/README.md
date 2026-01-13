# base-e2e

End-to-end testing library for node-reth flashblocks RPC.

## Overview

This library provides a reusable test framework for validating flashblocks RPC
implementations. It includes:

- **TestClient**: RPC client for connecting to node-reth instances
- **FlashblockHarness**: Test helper for running tests within flashblock windows
- **FlashblocksStream**: Direct WebSocket streaming from flashblocks endpoints
- **Test Suite**: Comprehensive test cases for flashblocks functionality

## Test Categories

- **blocks**: Block retrieval and pending state visibility
- **call**: `eth_call` and `eth_estimateGas` tests
- **receipts**: Transaction receipt retrieval
- **logs**: `eth_getLogs` including pending logs
- **subscriptions**: Flashblocks WebSocket endpoint validation
- **metering**: `base_meterBundle` and `base_meteredPriorityFeePerGas` endpoints
- **contracts**: Contract deployment and interaction tests

## Usage

### CLI Tool

Run the E2E tests against a live node:

```bash
PRIVATE_KEY=0x... cargo run --bin flashblocks-e2e -- \
  --rpc-url http://localhost:8545 \
  --flashblocks-ws-url wss://localhost:8546/ws \
  --recipient 0x...
```

Filter to specific tests:

```bash
cargo run --bin flashblocks-e2e -- \
  --rpc-url http://localhost:8545 \
  --flashblocks-ws-url wss://localhost:8546/ws \
  --filter "blocks*" \
  -vv
```

### Simulator Contract

For more meaningful state root timing results, deploy the Simulator contract
and run with the `--simulator` flag:

```bash
# Deploy Simulator (from base-benchmark repo)
forge create contracts/src/Simulator.sol:Simulator \
  --rpc-url $RPC_URL \
  --private-key $PRIVATE_KEY \
  --constructor-args 0 \
  --value 1ether

# Run with --simulator flag
PRIVATE_KEY=0x... cargo run --bin flashblocks-e2e -- \
  --rpc-url http://localhost:8545 \
  --flashblocks-ws-url wss://localhost:8546/ws \
  --recipient 0x... \
  --simulator 0x<deployed_address> \
  --filter "meter*" \
  -vv
```

### Library Usage

This library is used by the `flashblocks-e2e` binary. See `bin/flashblocks-e2e/`
for the CLI tool.

```rust,ignore
use base_testing_flashblocks_e2e::{TestClient, build_test_suite, run_tests};

let client = TestClient::new(rpc_url, flashblocks_ws_url, private_key, recipient, simulator).await?;
let suite = build_test_suite();
let results = run_tests(&client, &suite, None, false).await;
```

## Environment Variables

- `PRIVATE_KEY`: Hex-encoded private key for transaction-sending tests (optional)

## CLI Options

| Option | Env Var | Description |
|--------|---------|-------------|
| `--rpc-url` | `RPC_URL` | HTTP RPC endpoint URL |
| `--flashblocks-ws-url` | `FLASHBLOCKS_WS_URL` | Flashblocks WebSocket URL |
| `--recipient` | `RECIPIENT_ADDRESS` | Recipient address for ETH transfer tests |
| `--simulator` | `SIMULATOR_ADDRESS` | Simulator contract address for state root timing tests |
| `--filter` | - | Filter tests by name pattern |
| `-v`, `-vv` | - | Increase verbosity |
| `--list` | - | List available tests |
| `--json` | - | Output results as JSON |
