#!/bin/bash
# Test Account Abstraction RPC endpoints

set -e

RPC_URL="${RPC_URL:-http://localhost:8545}"

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${BLUE}Testing Account Abstraction RPC endpoints at $RPC_URL${NC}"
echo ""

# Test 1: eth_sendUserOperation (v0.6)
echo -e "${YELLOW}[1/6] Testing eth_sendUserOperation (v0.6)...${NC}"
curl -s -X POST $RPC_URL \
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
  }' | jq .
echo -e "${GREEN}✓ Sent v0.6 UserOperation${NC}"
echo ""

# Test 2: eth_sendUserOperation (v0.7)
echo -e "${YELLOW}[2/6] Testing eth_sendUserOperation (v0.7)...${NC}"
curl -s -X POST $RPC_URL \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "eth_sendUserOperation",
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
  }' | jq .
echo -e "${GREEN}✓ Sent v0.7 UserOperation${NC}"
echo ""

# Test 3: eth_estimateUserOperationGas (v0.6)
echo -e "${YELLOW}[3/6] Testing eth_estimateUserOperationGas (v0.6)...${NC}"
curl -s -X POST $RPC_URL \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "eth_estimateUserOperationGas",
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
  }' | jq .
echo -e "${GREEN}✓ Estimated gas for v0.6 UserOperation${NC}"
echo ""

# Test 4: eth_estimateUserOperationGas (v0.7)
echo -e "${YELLOW}[4/6] Testing eth_estimateUserOperationGas (v0.7)...${NC}"
curl -s -X POST $RPC_URL \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 4,
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
  }' | jq .
echo -e "${GREEN}✓ Estimated gas for v0.7 UserOperation${NC}"
echo ""

# Test 5: base_validateUserOperation (v0.6)
echo -e "${YELLOW}[5/6] Testing base_validateUserOperation (v0.6)...${NC}"
curl -s -X POST $RPC_URL \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 5,
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
  }' | jq .
echo -e "${GREEN}✓ Validated v0.6 UserOperation${NC}"
echo ""

# Test 6: eth_supportedEntryPoints
echo -e "${YELLOW}[6/6] Testing eth_supportedEntryPoints...${NC}"
curl -s -X POST $RPC_URL \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 6,
    "method": "eth_supportedEntryPoints",
    "params": []
  }' | jq .
echo -e "${GREEN}✓ Retrieved supported EntryPoints${NC}"
echo ""

echo -e "${BLUE}════════════════════════════════════════${NC}"
echo -e "${GREEN}✅ All tests completed!${NC}"
echo -e "${BLUE}════════════════════════════════════════${NC}"
echo ""
echo -e "${YELLOW}💡 Check your node logs to see the info!() messages${NC}"
echo -e "${YELLOW}   You should see logs for each UserOperation version detected${NC}"

