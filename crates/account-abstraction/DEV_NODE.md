# Running a Local Dev Node with Account Abstraction

This guide shows you how to start a fresh local development node with Account Abstraction (EIP-4337) RPC endpoints enabled.

## Quick Start

### 1. Build the node

```bash
cargo build --release -p base-reth-node
```

### 2. Initialize a fresh dev node

```bash
# Create a dev data directory
mkdir -p /tmp/reth-dev

# Initialize with dev genesis (creates a fresh local chain)
./target/release/base-reth-node init \
  --datadir /tmp/reth-dev \
  --chain dev
```

### 3. Start the node with Account Abstraction enabled

```bash
RUST_LOG=info ./target/release/base-reth-node node \
  --datadir /tmp/reth-dev \
  --chain dev \
  --http \
  --http.api all \
  --http.addr 0.0.0.0 \
  --http.port 8545 \
  --dev \
  --enable-account-abstraction
```

**Important flags:**
- `--dev` - Runs in dev mode (auto-mines blocks, no P2P)
- `--enable-account-abstraction` - Enables our EIP-4337 RPC endpoints (both `eth_*` and `base_*` methods)
- `--http.api all` - Exposes all standard RPC namespaces
- `RUST_LOG=info` - Shows logs including your `info!()` statements

### 4. Test the Account Abstraction endpoints

In another terminal, test your RPC endpoints:

#### Test `eth_sendUserOperation` (v0.6)

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "eth_sendUserOperation",
    "params": [{
      "sender": "0x1234567890123456789012345678901234567890",
      "nonce": "0x1",
      "initCode": "0xabcd",
      "callData": "0x1234",
      "callGasLimit": "0x5208",
      "verificationGasLimit": "0x5208",
      "preVerificationGas": "0x5208",
      "maxFeePerGas": "0x3b9aca00",
      "maxPriorityFeePerGas": "0x3b9aca00",
      "paymasterAndData": "0x",
      "signature": "0xabcdef"
    }, "0x5656565656565656565656565656565656565656"]
  }'
```

**You should see in the logs:**
```
INFO base_reth_account_abstraction::rpc: Got v0.6 UserOperation request for eth_sendUserOperation method="eth_sendUserOperation" version="v0.6" sender=0x1234567890123456789012345678901234567890 entry_point=0x5656565656565656565656565656565656565656
```

#### Test `eth_estimateUserOperationGas` (v0.7)

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "eth_estimateUserOperationGas",
    "params": [{
      "sender": "0x1234567890123456789012345678901234567890",
      "nonce": "0x1",
      "factory": "0x0000000000000000000000000000000000000000",
      "factoryData": "0x",
      "callData": "0x1234",
      "callGasLimit": "0x5208",
      "verificationGasLimit": "0x5208",
      "preVerificationGas": "0x5208",
      "maxFeePerGas": "0x3b9aca00",
      "maxPriorityFeePerGas": "0x3b9aca00",
      "paymaster": "0x0000000000000000000000000000000000000000",
      "paymasterVerificationGasLimit": "0x5208",
      "paymasterPostOpGasLimit": "0x5208",
      "paymasterData": "0x",
      "signature": "0xabcdef"
    }, "0x5656565656565656565656565656565656565656"]
  }'
```

**You should see in the logs:**
```
INFO base_reth_account_abstraction::rpc: Got v0.7 UserOperation request for eth_estimateUserOperationGas method="eth_estimateUserOperationGas" version="v0.7" sender=0x1234567890123456789012345678901234567890 entry_point=0x5656565656565656565656565656565656565656
```

#### Test `base_validateUserOperation` (v0.6)

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "base_validateUserOperation",
    "params": [{
      "sender": "0x1234567890123456789012345678901234567890",
      "nonce": "0x1",
      "initCode": "0xabcd",
      "callData": "0x1234",
      "callGasLimit": "0x5208",
      "verificationGasLimit": "0x5208",
      "preVerificationGas": "0x5208",
      "maxFeePerGas": "0x3b9aca00",
      "maxPriorityFeePerGas": "0x3b9aca00",
      "paymasterAndData": "0x",
      "signature": "0xabcdef"
    }, "0x5656565656565656565656565656565656565656"]
  }'
```

**You should see in the logs:**
```
INFO base_reth_account_abstraction::rpc: Got v0.6 UserOperation request for base_validateUserOperation method="base_validateUserOperation" version="v0.6" sender=0x1234567890123456789012345678901234567890 entry_point=0x5656565656565656565656565656565656565656
```

## Available RPC Endpoints

When `--enable-account-abstraction` is set, the following endpoints are available:

### Standard EIP-4337 endpoints (`eth` namespace):
- `eth_sendUserOperation` - Submit a UserOperation to the bundler
- `eth_estimateUserOperationGas` - Estimate gas for a UserOperation
- `eth_getUserOperationByHash` - Get UserOperation by hash
- `eth_getUserOperationReceipt` - Get UserOperation receipt
- `eth_supportedEntryPoints` - Get supported EntryPoint addresses

### Base-specific endpoints (`base` namespace):
- `base_validateUserOperation` - Validate a UserOperation without submitting

Both v0.6 and v0.7+ UserOperations are automatically detected based on the fields present in the request.

## Troubleshooting

### Port already in use
If port 8545 is already in use, change it:
```bash
--http.port 8546
```

### Can't see logs
Make sure `RUST_LOG=info` is set. For more verbose logs:
```bash
RUST_LOG=debug ./target/release/base-reth-node ...
```

### Want to start fresh?
Delete the data directory and re-initialize:
```bash
rm -rf /tmp/reth-dev
./target/release/base-reth-node init --datadir /tmp/reth-dev --chain dev
```

## Next Steps

1. Implement the TODO sections in `crates/account-abstraction/src/rpc.rs`
2. Add EntryPoint contract interaction
3. Implement bundler pool management
4. Add UserOperation validation logic

