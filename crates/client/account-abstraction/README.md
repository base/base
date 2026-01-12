# Account Abstraction RPC

RPC endpoints for EIP-4337 account abstraction bundler operations.

## Quick Start

### Run a Local Dev Node

The easiest way to test the Account Abstraction endpoints:

```bash
# Start a fresh local dev node with account abstraction enabled
./crates/account-abstraction/start_dev_node.sh

# In another terminal, test all the RPC endpoints
./crates/account-abstraction/test_rpc.sh
```

See [DEV_NODE.md](./DEV_NODE.md) for detailed instructions.

### Manual Integration

To enable account abstraction on any node:

```bash
cargo run -p base-reth-node -- \
  --enable-account-abstraction \
  --http \
  --http.api all
```

## Version Support

This implementation supports **both EIP-4337 v0.6 and v0.7+** specifications. Version detection is automatic based on the fields present in the JSON request:

- **v0.6**: Uses `initCode` and `paymasterAndData` fields
- **v0.7+**: Uses `factory`, `factoryData`, and separate paymaster fields

No version tagging is required - the API automatically detects and processes the correct version.

---

## RPC Endpoints

RPC endpoints for EIP-4337 account abstraction bundler operations. These are defined in (ERC-7769)[https://eips.ethereum.org/EIPS/eip-7769]

## `eth_sendUserOperation`

Submits a User Operation object to the bundler pool to be included in a future block.

**Parameters:**
- `user_operation`: UserOperation object
- `entry_point`: Address of the entry point contract

**Returns:**
- `user_operation_hash`: Hash of the user operation

## `eth_estimateUserOperationGas`

Estimates the gas values for a User Operation to be executed successfully.

**Parameters:**
- `user_operation`: UserOperation object
- `entry_point`: Address of the entry point contract

**Returns:**
- `pre_verification_gas`: Gas overhead for verification
- `verification_gas_limit`: Gas limit for verification
- `call_gas_limit`: Gas limit for execution

## `eth_getUserOperationByHash`

Returns a User Operation based on its hash.

**Parameters:**
- `user_operation_hash`: Hash of the user operation

**Returns:**
- User Operation object or null if not found

## `eth_getUserOperationReceipt`

Returns the receipt of a User Operation by its hash.

**Parameters:**
- `user_operation_hash`: Hash of the user operation

**Returns:**
- Receipt object containing execution details or null if not found

## `eth_supportedEntryPoints`

Returns an array of entry point addresses supported by the bundler.

**Parameters:**
- None

**Returns:**
- Array of entry point addresses

## `base_validateUserOperation`

Validates a User Operation without submitting it to the pool.

**Parameters:**
- `user_operation`: UserOperation object
- `entry_point`: Address of the entry point contract

**Returns:**
- `valid`: Boolean indicating if the operation is valid
- `reason`: Optional reason string if invalid

