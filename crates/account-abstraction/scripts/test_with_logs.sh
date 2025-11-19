#!/bin/bash
# Run test and then query logs with eth_getLogs
set -e

RPC_URL="http://localhost:8545"
ENTRYPOINT="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
USER_OP_EVENT_TOPIC="0x49628fd1471006c1482da88028e9ce4dbb080b815c9b0344d39e5a8e6ec1419f"

cd /Users/ericliu/Projects/node-reth/crates/account-abstraction

echo "═══════════════════════════════════════════════════════"
echo "    Run Test and Query with eth_getLogs"
echo "═══════════════════════════════════════════════════════"
echo ""

# Run the complete test
./scripts/complete_test.sh > /tmp/test_output.txt 2>&1

echo "[Step 1] Test completed, checking results..."
if grep -q "UserOperation executed successfully" /tmp/test_output.txt; then
    echo "✅ UserOperation executed"
else
    echo "❌ UserOperation failed"
    cat /tmp/test_output.txt
    exit 1
fi

TX_HASH=$(grep "TX Hash:" /tmp/test_output.txt | awk '{print $3}')
USER_OP_HASH=$(grep "UserOp Hash:" /tmp/test_output.txt | awk '{print $3}')

echo "   UserOp Hash: $USER_OP_HASH"
echo "   TX Hash: $TX_HASH"
echo ""

echo "[Step 2] Querying all EntryPoint logs with eth_getLogs..."
echo ""
echo "📝 Request:"
echo ""
cat << EOF
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "eth_getLogs",
  "params": [{
    "address": "$ENTRYPOINT",
    "topics": ["$USER_OP_EVENT_TOPIC"],
    "fromBlock": "0x0",
    "toBlock": "latest"
  }]
}
EOF

echo ""
echo "📊 Response:"
echo ""

LOGS=$(curl -s -X POST $RPC_URL \
  -H "Content-Type: application/json" \
  -d "{
    \"jsonrpc\": \"2.0\",
    \"id\": 1,
    \"method\": \"eth_getLogs\",
    \"params\": [{
      \"address\": \"$ENTRYPOINT\",
      \"topics\": [\"$USER_OP_EVENT_TOPIC\"],
      \"fromBlock\": \"0x0\",
      \"toBlock\": \"latest\"
    }]
  }")

echo "$LOGS" | jq .

echo ""
echo "[Step 3] Parsing UserOperation receipts from logs..."
echo ""

echo "$LOGS" | jq -r '.result[] | 
  "═══════════════════════════════════════════════════════\n" +
  "UserOperation Receipt:\n" +
  "  UserOp Hash:    " + .topics[1] + "\n" +
  "  Sender:         " + .topics[2] + "\n" +
  "  Paymaster:      " + .topics[3] + "\n" +
  "  Success:        " + (if (.data[66:130] == "0x0000000000000000000000000000000000000000000000000000000000000000") then "❌ false" else "✅ true" end) + "\n" +
  "  Actual Gas Cost:" + .data[130:194] + "\n" +
  "  Actual Gas Used:" + .data[194:258] + "\n" +
  "  Block:          " + .blockNumber + "\n" +
  "  TX Hash:        " + .transactionHash + "\n" +
  "═══════════════════════════════════════════════════════"
'

echo ""
echo "✅ This demonstrates how eth_getUserOperationReceipt would work:"
echo "   1. Query eth_getLogs filtered on EntryPoint + UserOperationEvent"
echo "   2. Find the log matching the userOpHash"
echo "   3. Parse the event data to construct the receipt"


