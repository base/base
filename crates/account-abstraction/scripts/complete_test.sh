#!/bin/bash
# Complete end-to-end test: Deploy everything and test eth_getUserOperationReceipt
set -e

RPC_URL="http://localhost:8545"
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
DEPLOYER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

echo "═══════════════════════════════════════════════════════"
echo "    Complete UserOperation Test with Receipt"
echo "═══════════════════════════════════════════════════════"
echo ""

# Check if node is running
echo "[1/7] Checking if node is running..."
if ! curl -s -X POST $RPC_URL -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' > /dev/null; then
    echo "❌ Node is not running at $RPC_URL"
    echo "Please start the node first:"
    echo "  ./crates/account-abstraction/start_dev_node.sh"
    exit 1
fi
echo "✅ Node is running"
echo ""

# Deploy contracts
echo "[2/7] Deploying contracts..."
cd /Users/ericliu/Projects/node-reth/crates/account-abstraction
./scripts/deploy_contracts.sh > /tmp/deploy_output.txt 2>&1
ENTRYPOINT="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
FACTORY=$(jq -r '.simpleAccountFactory' deployment-info.json)
WALLET=$(jq -r '.exampleWalletAddress' deployment-info.json | sed 's/0x0*/0x/')
echo "✅ Contracts deployed"
echo "   EntryPoint: $ENTRYPOINT"
echo "   Factory: $FACTORY"
echo "   Wallet: $WALLET"
echo ""

# Deploy wallet via factory
echo "[3/7] Deploying wallet..."
cast send $FACTORY "createAccount(address,uint256)" $DEPLOYER 0 \
  --private-key $DEPLOYER_KEY \
  --rpc-url $RPC_URL \
  --gas-limit 1000000 > /tmp/wallet_deploy.txt 2>&1
echo "✅ Wallet deployed at $WALLET"
echo ""

# Fund wallet
echo "[4/7] Funding wallet..."
cast send $WALLET --value 1ether \
  --private-key $DEPLOYER_KEY \
  --rpc-url $RPC_URL > /dev/null 2>&1
echo "✅ Wallet funded with 1 ETH"
echo ""

# Deposit to EntryPoint
echo "[5/7] Depositing to EntryPoint..."
cast send $ENTRYPOINT "depositTo(address)" $WALLET --value 0.5ether \
  --private-key $DEPLOYER_KEY \
  --rpc-url $RPC_URL > /dev/null 2>&1
echo "✅ Deposited 0.5 ETH to EntryPoint for wallet"
echo ""

# Submit UserOperation
echo "[6/7] Submitting UserOperation bundle..."
cargo run --quiet --example submit_bundle -- \
  --bundler-key $DEPLOYER_KEY \
  --owner-key $DEPLOYER_KEY \
  --sender $WALLET \
  --entry-point $ENTRYPOINT \
  --chain-id 31337 \
  --rpc-url $RPC_URL \
  --nonce 0 > /tmp/submit_output.txt 2>&1

# Extract UserOp hash and transaction hash
USER_OP_HASH=$(grep "UserOperation Hash:" /tmp/submit_output.txt | grep -oE "0x[a-fA-F0-9]{64}")
TX_HASH=$(grep "transactionHash" /tmp/submit_output.txt | grep -oE "0x[a-fA-F0-9]{64}" | head -1)
STATUS=$(grep "^status " /tmp/submit_output.txt | awk '{print $2}')

if [ "$STATUS" != "1" ]; then
    echo "❌ UserOperation failed!"
    cat /tmp/submit_output.txt
    exit 1
fi

echo "✅ UserOperation executed successfully!"
echo "   UserOp Hash: $USER_OP_HASH"
echo "   TX Hash: $TX_HASH"
echo ""

# Test eth_getUserOperationReceipt
echo "[7/7] Testing eth_getUserOperationReceipt..."
echo ""

RECEIPT=$(curl -s -X POST $RPC_URL \
  -H "Content-Type: application/json" \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getUserOperationReceipt\",\"params\":[\"$USER_OP_HASH\"]}")

echo "Response:"
echo "$RECEIPT" | jq .
echo ""

# Check if it worked
if echo "$RECEIPT" | jq -e '.result' > /dev/null 2>&1; then
    echo "═══════════════════════════════════════════════════════"
    echo "✅ SUCCESS! eth_getUserOperationReceipt is working!"
    echo "═══════════════════════════════════════════════════════"
    echo ""
    echo "Receipt details:"
    echo "$RECEIPT" | jq '.result'
elif echo "$RECEIPT" | jq -e '.error.code == -32601' > /dev/null 2>&1; then
    echo "═══════════════════════════════════════════════════════"
    echo "⚠️  Method not found - RPC endpoint not registered"
    echo "═══════════════════════════════════════════════════════"
    echo ""
    echo "The UserOperation executed successfully, but eth_getUserOperationReceipt"
    echo "is not available. Make sure you're running base-reth-node with"
    echo "--enable-account-abstraction flag."
    echo ""
    echo "Manual verification of receipt from transaction logs:"
    cast receipt $TX_HASH --rpc-url $RPC_URL --json | \
      jq '.logs[] | select(.topics[0] == "0x49628fd1471006c1482da88028e9ce4dbb080b815c9b0344d39e5a8e6ec1419f")'
else
    echo "❌ Unexpected response"
fi

