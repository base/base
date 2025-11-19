#!/bin/bash
# Demo eth_getLogs for querying UserOperation receipts

RPC_URL="http://localhost:8545"
ENTRYPOINT="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
USER_OP_EVENT_TOPIC="0x49628fd1471006c1482da88028e9ce4dbb080b815c9b0344d39e5a8e6ec1419f"

echo "═══════════════════════════════════════════════════════"
echo "   eth_getLogs Example for UserOperation Receipts"
echo "═══════════════════════════════════════════════════════"
echo ""

echo "This shows how to query UserOperation receipts using eth_getLogs:"
echo ""

echo "📝 Example Request:"
echo ""
cat << 'EOF' | jq .
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "eth_getLogs",
  "params": [{
    "address": "0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789",
    "topics": ["0x49628fd1471006c1482da88028e9ce4dbb080b815c9b0344d39e5a8e6ec1419f"],
    "fromBlock": "0x0",
    "toBlock": "latest"
  }]
}
EOF

echo ""
echo "This queries:"
echo "  • address: EntryPoint contract"
echo "  • topics[0]: UserOperationEvent signature"
echo "  • fromBlock/toBlock: Block range"
echo ""

echo "🎯 Example Response (when UserOperations exist):"
echo ""
cat << 'EOF' | jq .
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": [{
    "address": "0x5ff137d4b0fdcd49dca30c7cf57e578a026d2789",
    "topics": [
      "0x49628fd1471006c1482da88028e9ce4dbb080b815c9b0344d39e5a8e6ec1419f",
      "0xacf24496d4923a9414710c180a716c804b9c45d550dd5f8939918c4259324954",
      "0x00000000000000000000000063c6a4b674e37e9e4ac95675faab48982856799e",
      "0x0000000000000000000000000000000000000000000000000000000000000000"
    ],
    "data": "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000542ad8ae1879000000000000000000000000000000000000000000000000000000000001697f",
    "blockNumber": "0x577",
    "transactionHash": "0x6c78f0ade33f1f3eddf3fe9e84a792cddbb415d0aeb84f1e685f3d0750ea6fea",
    "logIndex": "0x1"
  }]
}
EOF

echo ""
echo "📊 Parsed Receipt Data:"
echo ""
echo "  UserOp Hash:     topics[1] = 0xacf2...3c68"
echo "  Sender:          topics[2] = 0x63c6...799e"  
echo "  Paymaster:       topics[3] = 0x0000...0000 (none)"
echo "  Nonce:           data[0:66]"
echo "  Success:         data[66:130] = 0x01 (true)"
echo "  Actual Gas Cost: data[130:194]"
echo "  Actual Gas Used: data[194:258]"
echo ""

echo "═══════════════════════════════════════════════════════"
echo "✅ How eth_getUserOperationReceipt uses this:"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "1. Client calls: eth_getUserOperationReceipt(userOpHash)"
echo "2. Server queries: eth_getLogs(EntryPoint, UserOperationEvent)"
echo "3. Server filters: logs where topics[1] == userOpHash"
echo "4. Server parses: event data into UserOperationReceipt struct"
echo "5. Server returns: formatted receipt to client"
echo ""

echo "🧪 Test it live:"
echo ""
echo "curl -X POST $RPC_URL \\"
echo "  -H 'Content-Type: application/json' \\"
echo "  -d '{"
echo "    \"jsonrpc\": \"2.0\","
echo "    \"id\": 1,"
echo "    \"method\": \"eth_getLogs\","
echo "    \"params\": [{"
echo "      \"address\": \"$ENTRYPOINT\","
echo "      \"topics\": [\"$USER_OP_EVENT_TOPIC\"],"
echo "      \"fromBlock\": \"0x0\","
echo "      \"toBlock\": \"latest\""
echo "    }]"
echo "  }' | jq ."


