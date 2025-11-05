# Account Abstraction RPC

RPC endpoints for EIP-4337 account abstraction bundler operations.

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

