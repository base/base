# Complete UserOperation Receipt Test

This guide walks you through testing the complete EIP-4337 flow: creating a UserOperation, submitting it directly to the EntryPoint, and retrieving the receipt.

## What This Tests

✅ UserOperation hash calculation  
✅ ECDSA signature generation  
✅ Direct EntryPoint.handleOps() call  
✅ UserOperationEvent emission  
✅ Receipt retrieval via `eth_getUserOperationReceipt`  

## Prerequisites

1. **Node running** with account abstraction enabled
2. **EntryPoint v0.6** deployed at `0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789`
3. **Smart contract wallet** deployed (from previous steps)
4. **Foundry** installed (for `cast` commands)

## Quick Test (Automated)

Run the complete test with one command:

```bash
cd /Users/ericliu/Projects/node-reth/crates/account-abstraction

./scripts/test_receipt.sh
```

This will:
1. ✅ Create a signed UserOperation
2. ✅ Bundle it and submit to EntryPoint
3. ✅ Wait for confirmation
4. ✅ Query the receipt via `eth_getUserOperationReceipt`

### Expected Output

```
═══════════════════════════════════════════════════════
    UserOperation Receipt Test
═══════════════════════════════════════════════════════

📋 Configuration:
   Wallet: 0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94
   EntryPoint: 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789
   Chain: 31337

═══════════════════════════════════════════════════════
[Step 1/3] Building and Submitting UserOperation Bundle
═══════════════════════════════════════════════════════

🚀 Creating and Submitting UserOperation Bundle

📝 UserOperation Hash: 0x1234...
✅ Signature: 0xabcd...

📦 Bundle Details:
   Operations: 1
   Beneficiary: 0x8568...
   EntryPoint: 0x5FF1...
   Call data size: 740 bytes

🔨 Submitting bundle to EntryPoint...

✅ Bundle submitted successfully!

blockHash            0x...
transactionHash      0x...
status               1 (success)

✓ Bundle submitted
✓ UserOperation Hash: 0x1234...

═══════════════════════════════════════════════════════
[Step 2/3] Waiting for Confirmation...
═══════════════════════════════════════════════════════

✓ Transaction should be mined

═══════════════════════════════════════════════════════
[Step 3/3] Retrieving UserOperation Receipt
═══════════════════════════════════════════════════════

Calling eth_getUserOperationReceipt...

Response:
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "userOpHash": "0x1234...",
    "entryPoint": "0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789",
    "sender": "0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94",
    "nonce": "0x0",
    "paymaster": "0x0000000000000000000000000000000000000000",
    "actualGasCost": "0x...",
    "actualGasUsed": "0x...",
    "success": true,
    "logs": [],
    "receipt": "0x"
  }
}

✅ Receipt Retrieved Successfully!

Receipt details:
{
  "userOpHash": "0x1234...",
  "sender": "0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94",
  "nonce": "0x0",
  "success": true,
  "actualGasCost": "0x...",
  "actualGasUsed": "0x..."
}

═══════════════════════════════════════════════════════
✅ Test Complete!
═══════════════════════════════════════════════════════
```

## Manual Step-by-Step Test

If you want to run each step manually:

### Step 1: Submit UserOperation Bundle

```bash
cd /Users/ericliu/Projects/node-reth/crates/account-abstraction

cargo run --example submit_bundle -- \
  --bundler-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --owner-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --sender 0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94 \
  --entry-point 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  --chain-id 31337 \
  --rpc-url http://localhost:8545 \
  --nonce 0
```

**Save the UserOperation Hash from the output!**

### Step 2: Query the Receipt

```bash
USER_OP_HASH="0x..."  # From step 1

curl -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d "{
    \"jsonrpc\": \"2.0\",
    \"id\": 1,
    \"method\": \"eth_getUserOperationReceipt\",
    \"params\": [\"$USER_OP_HASH\"]
  }" | jq
```

## What Happens Under the Hood

### 1. UserOperation Creation
```
User Input → Hash Calculation → ECDSA Signature → Signed UserOp
```

### 2. Bundle Submission
```
UserOp → EntryPoint.handleOps() → On-chain Execution
```

### 3. Event Emission
```
EntryPoint → UserOperationEvent(hash, sender, paymaster, ...) → Blockchain
```

### 4. Receipt Lookup
```
RPC Call → Search Blocks for Event → Parse Event → Return Receipt
```

## Troubleshooting

### "No receipt found yet"

**Possible causes:**
1. Transaction not mined yet - wait a few seconds and retry
2. UserOperation failed validation in EntryPoint
3. Wallet not properly funded
4. Signature invalid

**Debug:**
```bash
# Check if transaction was mined
cast receipt <TX_HASH> --rpc-url http://localhost:8545

# Check EntryPoint logs
cast logs --address 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 --rpc-url http://localhost:8545
```

### "Transaction reverted"

**Common reasons:**
1. **AA23**: Wallet doesn't have enough gas
   ```bash
   cast balance 0x8568D5CE3dA8EbebA9380bdEBf0494d9a9847E94 --rpc-url http://localhost:8545
   ```

2. **AA24**: Signature validation failed
   - Check you're using the correct owner private key
   - Verify hash calculation matches EntryPoint's

3. **AA10**: Wallet already deployed but initCode provided
   - Remove `--init-code` flag if wallet exists

### "Receipt shows success: false"

The UserOperation was accepted but failed during execution:
- Check the `actualGasCost` to see if it ran out of gas
- The wallet's `execute()` function may have reverted
- Check node logs for detailed error messages

## Verification

To verify everything worked:

```bash
# 1. Check if UserOperationEvent was emitted
cast logs \
  --address 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  --rpc-url http://localhost:8545 | jq

# 2. Query by specific UserOp hash
USER_OP_HASH="0x..."
cast logs \
  --address 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  "UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)" \
  $USER_OP_HASH \
  --rpc-url http://localhost:8545
```

## What's Tested

| Component | Status |
|-----------|--------|
| UserOperation signing | ✅ Tested |
| Hash calculation (v0.6) | ✅ Tested |
| EntryPoint.handleOps() | ✅ Tested |
| UserOperationEvent emission | ✅ Tested |
| eth_getUserOperationReceipt | ✅ Tested |
| Receipt parsing | ✅ Tested |
| Event log searching | ✅ Tested |

## Next Steps

Now that you have the complete flow working:

1. **Add validation** - Verify signatures before accepting UserOps
2. **Implement eth_sendUserOperation properly** - Store in mempool and bundle automatically
3. **Add batch bundling** - Bundle multiple UserOps in one transaction
4. **Support v0.7** - Add PackedUserOperation support
5. **Add paymaster support** - Let third parties sponsor gas

## Files Reference

- `examples/submit_bundle.rs` - Direct bundling tool
- `scripts/test_receipt.sh` - Automated test script
- `src/rpc.rs` - RPC implementation with receipt lookup
- `contracts/SimpleAccount.sol` - Smart contract wallet
- `contracts/IEntryPoint.sol` - EntryPoint interface

---

**Success!** 🎉 You now have a working EIP-4337 UserOperation flow with receipt retrieval!


