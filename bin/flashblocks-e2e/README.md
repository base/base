# flashblocks-e2e

End-to-end regression testing tool for node-reth flashblocks RPC.

## Overview

This tool runs a comprehensive suite of tests against a live node to validate
the flashblocks RPC implementation including state visibility, eth_call,
transaction receipts, WebSocket subscriptions, and metering endpoints.

## Usage

```bash
# Run all tests against a local node
flashblocks-e2e --rpc-url http://localhost:8545 --flashblocks-ws-url wss://localhost:8546/ws

# Run with a filter
flashblocks-e2e --filter "flashblock*"

# List available tests
flashblocks-e2e --list

# Continue running after failures
flashblocks-e2e --keep-going

# Verbose output
flashblocks-e2e -v

# JSON output for CI
flashblocks-e2e --format json
```

## Environment Variables

- `PRIVATE_KEY`: Hex-encoded private key for transaction-sending tests (optional)

## Test Categories

- **blocks**: Block retrieval and pending state visibility
- **call**: `eth_call` and `eth_estimateGas` tests
- **receipts**: Transaction receipt retrieval
- **logs**: `eth_getLogs` including pending logs
- **sanity**: Flashblocks WebSocket endpoint validation
- **metering**: `base_meterBundle` and `base_meteredPriorityFeePerGas` endpoints
- **contracts**: Contract deployment and interaction tests

## Exit Codes

- `0`: All tests passed
- `1`: One or more tests failed
