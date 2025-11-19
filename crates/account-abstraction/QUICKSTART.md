# Quick Start: Creating and Sending a UserOperation

This guide will walk you through creating a real, signed UserOperation and submitting it to your node with a deployed EntryPoint.

## Overview

We'll do the following:
1. Deploy a SimpleAccount factory contract
2. Calculate the wallet address  
3. Fund the wallet
4. Create and sign a UserOperation
5. Submit it to your node

## Prerequisites

- **Foundry** installed (`forge`, `cast`)
- **Node running** with EntryPoint v0.6 deployed at `0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789`
- **Funded account** (default: first anvil account)

## Step-by-Step Example

### 1. Start Your Node

```bash
cd /Users/ericliu/Projects/node-reth

# Build the node
cargo build --release -p base-reth-node

# Start with account abstraction enabled
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

### 2. Deploy the Smart Contract Wallet Factory

```bash
cd crates/account-abstraction

# Set environment variables
export RPC_URL=http://localhost:8545
export ENTRYPOINT_ADDRESS=0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789
export DEPLOYER_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

# Run deployment script
./scripts/deploy_contracts.sh
```

This will output something like:

```
✓ SimpleAccountFactory deployed at: 0x5FbDB2315678afecb367f032d93F642f64180aa3
✓ Wallet address (owner=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266, salt=0): 0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0
```

**Save these addresses!** You'll need them.

### 3. Fund the Wallet

The wallet needs ETH to pay for gas:

```bash
# Use the wallet address from step 2
WALLET_ADDRESS="0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0"

# Send 1 ETH to the wallet
cast send $WALLET_ADDRESS \
  --value 1ether \
  --private-key $DEPLOYER_PRIVATE_KEY \
  --rpc-url $RPC_URL

# Verify balance
cast balance $WALLET_ADDRESS --rpc-url $RPC_URL
```

### 4. Create a Signed UserOperation

Use our Rust tool to generate a properly signed UserOperation:

```bash
# Get chain ID
CHAIN_ID=$(cast chain-id --rpc-url $RPC_URL)

# The private key of the wallet owner (same as DEPLOYER_PRIVATE_KEY for this example)
OWNER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

# Create UserOperation
cargo run --example create_userop -- \
  --private-key $OWNER_KEY \
  --sender $WALLET_ADDRESS \
  --entry-point $ENTRYPOINT_ADDRESS \
  --chain-id $CHAIN_ID \
  --nonce 0
```

This outputs:
- The UserOperation hash
- The signature
- Complete JSON ready to send

Example output:

```json
{
  "sender": "0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0",
  "nonce": "0x0",
  "initCode": "0x",
  "callData": "0x",
  "callGasLimit": "0x186a0",
  "verificationGasLimit": "0x493e0",
  "preVerificationGas": "0xc350",
  "maxFeePerGas": "0x77359400",
  "maxPriorityFeePerGas": "0x3b9aca00",
  "paymasterAndData": "0x",
  "signature": "0x..."
}
```

### 5. Submit the UserOperation

Copy the JSON output and submit it:

```bash
curl -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "eth_sendUserOperation",
    "params": [
      {
        "sender": "0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0",
        "nonce": "0x0",
        "initCode": "0x",
        "callData": "0x",
        "callGasLimit": "0x186a0",
        "verificationGasLimit": "0x493e0",
        "preVerificationGas": "0xc350",
        "maxFeePerGas": "0x77359400",
        "maxPriorityFeePerGas": "0x3b9aca00",
        "paymasterAndData": "0x",
        "signature": "0xYOUR_SIGNATURE_HERE"
      },
      "0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
    ]
  }'
```

You should see:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": "0xUSEROPERATION_HASH"
}
```

### 6. Check the Receipt (Optional)

Once the UserOperation is mined:

```bash
USER_OP_HASH="0x..."  # From step 5

curl -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "eth_getUserOperationReceipt",
    "params": ["'$USER_OP_HASH'"]
  }' | jq
```

## Advanced: Deploying the Wallet with InitCode

If the wallet isn't deployed yet, you can deploy it with the first UserOperation:

```bash
# Get factory address from deployment-info.json
FACTORY_ADDRESS="0x5FbDB2315678afecb367f032d93F642f64180aa3"
OWNER_ADDRESS="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
SALT="0"

# Encode the factory call: createAccount(address owner, uint256 salt)
INIT_DATA=$(cast abi-encode "createAccount(address,uint256)" $OWNER_ADDRESS $SALT)

# Combine factory address + calldata (remove 0x from calldata)
INIT_CODE="${FACTORY_ADDRESS}${INIT_DATA:2}"

# Create UserOperation with initCode
cargo run --example create_userop -- \
  --private-key $OWNER_KEY \
  --sender $WALLET_ADDRESS \
  --entry-point $ENTRYPOINT_ADDRESS \
  --chain-id $CHAIN_ID \
  --nonce 0 \
  --init-code $INIT_CODE
```

## Advanced: Making a Transaction

To have the wallet execute a call (e.g., transfer tokens):

```bash
# Example: Call some contract
TARGET="0x1234567890123456789012345678901234567890"
VALUE="0"

# Encode the function you want to call
FUNC_DATA=$(cast calldata "someFunction(uint256)" 42)

# Encode as SimpleAccount.execute(address dest, uint256 value, bytes func)
CALL_DATA=$(cast abi-encode "execute(address,uint256,bytes)" $TARGET $VALUE $FUNC_DATA)

# Create UserOperation with callData
cargo run --example create_userop -- \
  --private-key $OWNER_KEY \
  --sender $WALLET_ADDRESS \
  --entry-point $ENTRYPOINT_ADDRESS \
  --chain-id $CHAIN_ID \
  --nonce 1 \
  --call-data $CALL_DATA
```

## Troubleshooting

### "AA23 reverted (or OOG)"
- Wallet doesn't have enough gas
- Try: `cast send $WALLET_ADDRESS --value 1ether ...`

### "AA24 signature error"
- Wrong private key used
- Make sure you're using the wallet owner's private key

### "AA10 sender already constructed"
- Remove `--init-code` flag if wallet is already deployed

### "Invalid signature"
- Hash calculation may be wrong
- Check that chain ID matches: `cast chain-id --rpc-url $RPC_URL`

## What's Next?

Now you have a working UserOperation flow! You can:

1. **Implement bundling** - Collect multiple UserOperations and submit them as a bundle
2. **Add validation** - Implement proper signature and nonce checking in the RPC handler
3. **Support paymasters** - Allow third parties to sponsor gas
4. **Add v0.7 support** - Extend to the latest EIP-4337 version

See [USEROP_GUIDE.md](./USEROP_GUIDE.md) for more detailed information.

## Files Reference

- `contracts/SimpleAccount.sol` - The smart contract wallet
- `contracts/SimpleAccountFactory.sol` - Factory for deploying wallets
- `contracts/IEntryPoint.sol` - EntryPoint interface
- `scripts/deploy_contracts.sh` - Deployment script
- `examples/create_userop.rs` - Tool to generate signed UserOperations
- `deployment-info.json` - Saved deployment addresses (generated)

## RPC Endpoints Available

- `eth_sendUserOperation` - Submit a UserOperation
- `eth_estimateUserOperationGas` - Estimate gas for a UserOperation
- `eth_getUserOperationByHash` - Get UserOperation by hash
- `eth_getUserOperationReceipt` - Get UserOperation receipt
- `eth_supportedEntryPoints` - Get supported EntryPoint addresses
- `base_validateUserOperation` - Validate without submitting


