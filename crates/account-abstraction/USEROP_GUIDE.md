# Creating and Submitting User Operations

This guide shows you how to deploy a smart contract wallet and create valid, signed user operations to submit to your node.

## Prerequisites

1. **Foundry** - For contract deployment
   ```bash
   curl -L https://foundry.paradigm.xyz | bash
   foundryup
   ```

2. **Running node** with EntryPoint v0.6 deployed
   - EntryPoint v0.6 address: `0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789`

3. **Funded account** - You need ETH to deploy contracts and fund the wallet

## Step 1: Deploy Smart Contract Wallet

We've provided a SimpleAccount implementation and factory contract.

```bash
cd crates/account-abstraction

# Deploy the factory (EntryPoint must already be deployed)
export RPC_URL=http://localhost:8545
export ENTRYPOINT_ADDRESS=0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789

# Use the default anvil private key or your own
export DEPLOYER_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

./scripts/deploy_contracts.sh
```

This will:
1. Compile the contracts
2. Deploy `SimpleAccountFactory`
3. Calculate the counterfactual address for your wallet
4. Save deployment info to `deployment-info.json`

## Step 2: Fund the Wallet

Before you can submit user operations, the wallet needs:
1. **ETH balance** (for gas)
2. **Deposit in EntryPoint** (for gas prefunding)

```bash
# From the deployment script output, use the wallet address
WALLET_ADDRESS="0x..."  # From deployment-info.json

# Send ETH to the wallet
cast send $WALLET_ADDRESS \
  --value 1ether \
  --private-key $DEPLOYER_PRIVATE_KEY \
  --rpc-url $RPC_URL

# Deposit into EntryPoint (if wallet is already deployed)
# The wallet contract has an addDeposit() function you can call
```

## Step 3: Create a Signed UserOperation

Use our Rust tool to create and sign a user operation:

```bash
cargo run --example create_userop -- \
  --private-key ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --sender 0x... \  # Your wallet address
  --entry-point 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  --chain-id 1337 \  # Use your actual chain ID
  --nonce 0
```

This will output:
- The UserOperation hash
- The signature
- Complete JSON ready to send

### Creating UserOp with Init Code (First Transaction)

If the wallet isn't deployed yet, include init code:

```bash
# Get the factory address from deployment-info.json
FACTORY_ADDRESS="0x..."
OWNER_ADDRESS="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
SALT="0"

# Encode the factory call: createAccount(address owner, uint256 salt)
INIT_CODE=$(cast abi-encode "createAccount(address,uint256)" $OWNER_ADDRESS $SALT)
INIT_CODE="${FACTORY_ADDRESS}${INIT_CODE:2}"  # Concatenate factory address + calldata

cargo run --example create_userop -- \
  --private-key ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --sender 0x... \
  --entry-point 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  --chain-id 1337 \
  --nonce 0 \
  --init-code $INIT_CODE
```

### Making a Transaction

To have the wallet execute a call, encode it in the callData:

```bash
# Example: Call some contract's transfer function
TARGET="0x..."
VALUE="0"
FUNC_DATA=$(cast calldata "transfer(address,uint256)" 0xRecipient 1000000)

# Encode as SimpleAccount.execute(address dest, uint256 value, bytes func)
CALL_DATA=$(cast abi-encode "execute(address,uint256,bytes)" $TARGET $VALUE $FUNC_DATA)

cargo run --example create_userop -- \
  --private-key ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --sender 0x... \
  --entry-point 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  --chain-id 1337 \
  --nonce 1 \
  --call-data $CALL_DATA
```

## Step 4: Submit the UserOperation

Use the JSON output from step 3 to submit via RPC:

```bash
curl -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "eth_sendUserOperation",
    "params": [
      {
        "sender": "0x...",
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
      },
      "0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
    ]
  }'
```

The response will contain the UserOperation hash:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": "0x..."
}
```

## Step 5: Check the UserOperation Receipt

```bash
USER_OP_HASH="0x..."  # From the sendUserOperation response

curl -X POST http://localhost:8545 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "eth_getUserOperationReceipt",
    "params": ["'$USER_OP_HASH'"]
  }'
```

## Troubleshooting

### "AA23 reverted (or OOG)"
- The wallet doesn't have enough gas
- Increase `callGasLimit` or fund the wallet

### "AA24 signature error"
- Wrong private key
- Hash calculation error
- Make sure you're using the wallet owner's key

### "AA10 sender already constructed"
- Remove `initCode` if the wallet is already deployed

### "insufficient funds for gas"
- The wallet needs ETH balance
- Or needs deposit in the EntryPoint

## Full Example

```bash
# 1. Deploy contracts
./scripts/deploy_contracts.sh

# 2. Get wallet address from deployment-info.json
WALLET=$(jq -r '.exampleWalletAddress' deployment-info.json)
OWNER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

# 3. Fund the wallet
cast send $WALLET --value 1ether --private-key $OWNER_KEY --rpc-url http://localhost:8545

# 4. Create UserOperation
cargo run --example create_userop -- \
  --private-key $OWNER_KEY \
  --sender $WALLET \
  --entry-point 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789 \
  --chain-id 1337 \
  --nonce 0 \
  > userop.json

# 5. Submit it
# (Copy the curl command from the create_userop output)
```

## Next Steps

- Implement bundling in the RPC handler
- Add validation logic
- Support paymasters
- Add v0.7 support


