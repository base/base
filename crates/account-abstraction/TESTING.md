# Testing Your UserOperation Flow

You now have a working UserOperation infrastructure! Here's how to test it end-to-end.

## What's Implemented

✅ **UserOperation Hash Calculation** - Proper EIP-4337 v0.6 hash calculation  
✅ **RPC Handler** - `eth_sendUserOperation` accepts and stores UserOperations  
✅ **Mempool** - Pending UserOperations are stored in memory  
✅ **Bundling Endpoint** - `base_bundleNow` to trigger bundling (for testing)  

## Quick Test Flow

### 1. Make Sure Your Node is Running

```bash
RUST_LOG=info ./target/release/base-reth-node node \
  --datadir /tmp/reth-dev \
  --chain dev \
  --http \
  --http.api all \
  --dev \
  --enable-account-abstraction
```

### 2. Create a Signed UserOperation

```bash
cd /Users/ericliu/Projects/node-reth/crates/account-abstraction

cargo run --example create_userop -- \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --sender 0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94 \
  --entry-point 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  --chain-id 31337 \
  --nonce 0
```

**Output:**
```
📝 UserOperation Hash: 0x1234...
✅ Signature: 0xabcd...

📦 Complete UserOperation:
{
  "sender": "0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94",
  "nonce": "0x0",
  ...
}
```

### 3. Submit the UserOperation

```bash
curl -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "eth_sendUserOperation",
    "params": [
      {
        "sender": "0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94",
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
  }' | jq
```

**Expected Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": "0xUSEROPERATION_HASH"
}
```

### 4. Check Your Node Logs

You should see:
```
INFO base_reth_account_abstraction::rpc: Received sendUserOperation (v0.6)
    sender=0x8568D5CE... entry_point=0x5FF137D4... nonce=0
INFO base_reth_account_abstraction::rpc: Calculated UserOperation hash
    hash=0x...
INFO base_reth_account_abstraction::rpc: UserOperation added to pending pool
    pending_count=1
INFO base_reth_account_abstraction::rpc: UserOperation accepted, will be bundled soon
```

### 5. Trigger Bundling (Test Method)

```bash
curl -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "base_bundleNow",
    "params": []
  }' | jq
```

**Node Logs:**
```
INFO base_reth_account_abstraction::rpc: Received bundle_now request
INFO base_reth_account_abstraction::rpc: Bundling UserOperations count=1
INFO base_reth_account_abstraction::rpc: Would bundle UserOperation
    sender=0x8568D5CE... entry_point=0x5FF137D4...
WARN base_reth_account_abstraction::rpc: Bundle creation not yet implemented
```

## What Happens Now?

✅ **UserOperations are accepted** and their hash is calculated correctly  
✅ **Stored in memory** waiting to be bundled  
✅ **Logs show the flow** through the system  

❌ **Not yet implemented:**
- Actually calling `EntryPoint.handleOps()` to execute the bundle
- Transaction creation and signing
- Automatic bundling (currently manual via `base_bundleNow`)

## Next Steps to Complete the Flow

### 1. Implement Actual Bundle Submission

In `rpc.rs`, the `bundle_now` method needs to:

```rust
// 1. Convert UserOperations to EntryPoint format
let ops: Vec<UserOperationStruct> = pending_ops
    .iter()
    .filter_map(|(op, _)| match op {
        UserOperation::V06(v06) => Some(convert_to_struct(v06)),
        _ => None,
    })
    .collect();

// 2. Encode the handleOps call
let call_data = handleOpsCall { ops, beneficiary }.abi_encode();

// 3. Create and sign transaction
let tx = TransactionRequest::default()
    .to(entry_point)
    .data(call_data)
    .from(bundler_address);

// 4. Submit to network
let tx_hash = eth_api.send_transaction(tx).await?;

```

### 2. Add Validation

Before accepting UserOperations:
- Verify signatures match the wallet owner
- Check nonces are valid
- Simulate execution via `eth_call`
- Verify gas limits are sufficient

### 3. Add Automatic Bundling

Instead of manual `base_bundleNow`:
- Trigger bundling every N seconds
- Or when mempool reaches X operations
- Or when gas prices are favorable

## Testing Without Full Implementation

You can still test the complete flow by:

1. **Manually calling EntryPoint** with `cast`:
   ```bash
   # Get the UserOperation data
   # Encode it for handleOps
   # Send directly with cast send
   ```

2. **Using a real bundler** like Rundler to process your UserOperations

3. **Testing with EntryPoint simulations** to verify your UserOperations are valid

## Summary

You now have:
- ✅ Smart contract wallet deployed
- ✅ Tools to create signed UserOperations
- ✅ RPC endpoint that accepts UserOperations
- ✅ Hash calculation working correctly
- ✅ In-memory mempool storing pending operations

The infrastructure is in place - you just need to wire up the final step of actually submitting bundles to the EntryPoint contract!


