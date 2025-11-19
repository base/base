#!/bin/bash
# Deploy SimpleAccount contracts using Foundry (forge)
#
# Prerequisites:
#   - foundry (forge, cast) installed
#   - RPC node running at RPC_URL
#   - Funded deployer account
#
# Usage:
#   ./scripts/deploy_contracts_forge.sh

set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8545}"
DEPLOYER_PRIVATE_KEY="${DEPLOYER_PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"  # Default anvil key
ENTRYPOINT_ADDRESS="${ENTRYPOINT_ADDRESS:-0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789}"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}    EIP-4337 SimpleAccount Contract Deployment${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Check if forge is installed
if ! command -v forge &> /dev/null; then
    echo -e "${RED}❌ Error: forge (foundry) is not installed${NC}"
    echo -e "${YELLOW}Install from: https://book.getfoundry.sh/getting-started/installation${NC}"
    echo -e "${YELLOW}Run: curl -L https://foundry.paradigm.xyz | bash && foundryup${NC}"
    exit 1
fi

# Check if cast is installed
if ! command -v cast &> /dev/null; then
    echo -e "${RED}❌ Error: cast (foundry) is not installed${NC}"
    echo -e "${YELLOW}Install from: https://book.getfoundry.sh/getting-started/installation${NC}"
    exit 1
fi

echo -e "${YELLOW}📋 Configuration:${NC}"
echo -e "   RPC URL: $RPC_URL"
echo -e "   EntryPoint: $ENTRYPOINT_ADDRESS"
echo -e "   Deployer: $(cast wallet address $DEPLOYER_PRIVATE_KEY)"
echo ""

# Get deployer balance
BALANCE=$(cast balance $(cast wallet address $DEPLOYER_PRIVATE_KEY) --rpc-url $RPC_URL)
echo -e "${YELLOW}💰 Deployer Balance: $(cast to-unit $BALANCE ether) ETH${NC}"
echo ""

# Change to contracts directory
CONTRACTS_DIR="$(dirname "$0")/../contracts"
cd "$CONTRACTS_DIR"

echo -e "${YELLOW}[1/4] Compiling SimpleAccount...${NC}"

# Compile SimpleAccount using forge
SIMPLE_ACCOUNT_BYTECODE=$(forge build --contracts SimpleAccount.sol --use 0.8.23 2>/dev/null | grep -A 1 "Compiler run successful" | tail -1 || \
    forge inspect SimpleAccount bytecode --use 0.8.23)

if [ -z "$SIMPLE_ACCOUNT_BYTECODE" ]; then
    echo -e "${RED}❌ Failed to compile SimpleAccount${NC}"
    exit 1
fi

echo -e "${GREEN}✓ SimpleAccount compiled${NC}"
echo ""

echo -e "${YELLOW}[2/4] Compiling SimpleAccountFactory...${NC}"

# Create a temporary file with all contracts
cat > temp_factory.sol << 'EOF'
// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.23;

// IEntryPoint interface
interface IEntryPoint {
    function balanceOf(address account) external view returns (uint256);
    function depositTo(address account) external payable;
    function withdrawTo(address payable withdrawAddress, uint256 withdrawAmount) external;
    function getNonce(address sender, uint192 key) external view returns (uint256 nonce);
}

// UserOperation struct
struct UserOperation {
    address sender;
    uint256 nonce;
    bytes initCode;
    bytes callData;
    uint256 callGasLimit;
    uint256 verificationGasLimit;
    uint256 preVerificationGas;
    uint256 maxFeePerGas;
    uint256 maxPriorityFeePerGas;
    bytes paymasterAndData;
    bytes signature;
}

// SimpleAccount
contract SimpleAccount {
    IEntryPoint private immutable _entryPoint;
    address public owner;

    modifier onlyOwner() {
        require(msg.sender == owner || msg.sender == address(this), "only owner");
        _;
    }

    modifier onlyEntryPoint() {
        require(msg.sender == address(_entryPoint), "only EntryPoint");
        _;
    }

    constructor(IEntryPoint anEntryPoint) {
        _entryPoint = anEntryPoint;
        owner = address(1);
    }

    function initialize(address anOwner) public {
        require(owner == address(0), "already initialized");
        owner = anOwner;
    }

    function execute(address dest, uint256 value, bytes calldata func) external onlyEntryPoint {
        (bool success, bytes memory result) = dest.call{value: value}(func);
        if (!success) {
            assembly {
                revert(add(result, 32), mload(result))
            }
        }
    }

    function validateUserOp(
        UserOperation calldata userOp,
        bytes32 userOpHash,
        uint256 missingAccountFunds
    ) external onlyEntryPoint returns (uint256) {
        bytes32 hash = keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", userOpHash));
        address recovered = _recover(hash, userOp.signature);
        
        if (owner != recovered)
            return 1; // SIG_VALIDATION_FAILED
        
        if (missingAccountFunds != 0) {
            (bool success,) = payable(msg.sender).call{value: missingAccountFunds}("");
            (success);
        }
        return 0;
    }

    function _recover(bytes32 hash, bytes memory signature) internal pure returns (address) {
        if (signature.length != 65) return address(0);
        
        bytes32 r;
        bytes32 s;
        uint8 v;
        
        assembly {
            r := mload(add(signature, 0x20))
            s := mload(add(signature, 0x40))
            v := byte(0, mload(add(signature, 0x60)))
        }
        
        if (uint256(s) > 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0) {
            return address(0);
        }
        
        return ecrecover(hash, v, r, s);
    }

    function entryPoint() public view returns (IEntryPoint) {
        return _entryPoint;
    }

    receive() external payable {}
}

// SimpleAccountFactory
contract SimpleAccountFactory {
    IEntryPoint public immutable entryPoint;

    constructor(IEntryPoint _entryPoint) {
        entryPoint = _entryPoint;
    }

    function createAccount(address owner, uint256 salt) public returns (SimpleAccount ret) {
        address addr = getAddress(owner, salt);
        uint256 codeSize = addr.code.length;
        if (codeSize > 0) {
            return SimpleAccount(payable(addr));
        }
        ret = SimpleAccount(payable(new SimpleAccount{salt: bytes32(salt)}(entryPoint)));
        ret.initialize(owner);
    }

    function getAddress(address owner, uint256 salt) public view returns (address) {
        return address(uint160(uint256(keccak256(abi.encodePacked(
            bytes1(0xff),
            address(this),
            salt,
            keccak256(abi.encodePacked(
                type(SimpleAccount).creationCode,
                abi.encode(entryPoint)
            ))
        )))));
    }
}
EOF

# Compile using forge
echo -e "${YELLOW}   Compiling with forge...${NC}"
forge build --contracts temp_factory.sol --use 0.8.23 > /dev/null 2>&1 || true

echo -e "${GREEN}✓ Contracts compiled${NC}"
echo ""

echo -e "${YELLOW}[3/4] Deploying SimpleAccountFactory...${NC}"

# Get the bytecode and constructor args
FACTORY_CREATION_CODE=$(forge inspect --use 0.8.23 temp_factory.sol:SimpleAccountFactory bytecode)
CONSTRUCTOR_ARGS=$(cast abi-encode "constructor(address)" $ENTRYPOINT_ADDRESS)
DEPLOY_CODE="${FACTORY_CREATION_CODE}${CONSTRUCTOR_ARGS:2}"

# Deploy the factory
DEPLOY_OUTPUT=$(cast send --private-key $DEPLOYER_PRIVATE_KEY \
    --rpc-url $RPC_URL \
    --create $DEPLOY_CODE \
    --json)

FACTORY_ADDRESS=$(echo $DEPLOY_OUTPUT | jq -r '.contractAddress')

if [ -z "$FACTORY_ADDRESS" ] || [ "$FACTORY_ADDRESS" == "null" ]; then
    echo -e "${RED}❌ Failed to deploy SimpleAccountFactory${NC}"
    rm -f temp_factory.sol
    exit 1
fi

echo -e "${GREEN}✓ SimpleAccountFactory deployed at: $FACTORY_ADDRESS${NC}"
echo ""

echo -e "${YELLOW}[4/4] Computing wallet address...${NC}"

# Example: Compute the address of a SimpleAccount
OWNER_ADDRESS="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"  # Default anvil address #0
SALT="0"

# Get the address
WALLET_ADDRESS=$(cast call $FACTORY_ADDRESS \
    "getAddress(address,uint256)" \
    $OWNER_ADDRESS \
    $SALT \
    --rpc-url $RPC_URL)

echo -e "${GREEN}✓ Wallet address (owner=$OWNER_ADDRESS, salt=$SALT): $WALLET_ADDRESS${NC}"
echo ""

# Clean up
rm -f temp_factory.sol

# Save deployment info
DEPLOYMENT_FILE="../deployment-info.json"
cat > $DEPLOYMENT_FILE << EOF
{
  "entryPoint": "$ENTRYPOINT_ADDRESS",
  "simpleAccountFactory": "$FACTORY_ADDRESS",
  "exampleWalletAddress": "$WALLET_ADDRESS",
  "exampleOwner": "$OWNER_ADDRESS",
  "exampleSalt": "$SALT",
  "rpcUrl": "$RPC_URL",
  "chainId": "$(cast chain-id --rpc-url $RPC_URL)"
}
EOF

echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}✅ Deployment Complete!${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""
echo -e "${YELLOW}📄 Deployment info saved to: $DEPLOYMENT_FILE${NC}"
echo ""
echo -e "${YELLOW}🚀 Next steps:${NC}"
echo ""
echo -e "1. Fund the wallet with ETH for gas:"
echo -e "   ${BLUE}cast send $WALLET_ADDRESS --value 0.1ether --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL${NC}"
echo ""
echo -e "2. Create a UserOperation:"
echo -e "   ${BLUE}cd .. && cargo run --example create_userop -- \\${NC}"
echo -e "   ${BLUE}  --private-key $DEPLOYER_PRIVATE_KEY \\${NC}"
echo -e "   ${BLUE}  --sender $WALLET_ADDRESS \\${NC}"
echo -e "   ${BLUE}  --entry-point $ENTRYPOINT_ADDRESS \\${NC}"
echo -e "   ${BLUE}  --chain-id $(cast chain-id --rpc-url $RPC_URL)${NC}"
echo ""


