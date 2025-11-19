#!/bin/bash
# Deploy SimpleAccount contracts using forge create
#
# Prerequisites:
#   - foundry (forge, cast) installed
#   - RPC node running at RPC_URL
#   - Funded deployer account
#
# Usage:
#   ./scripts/deploy_with_forge.sh

set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8545}"
DEPLOYER_PRIVATE_KEY="${DEPLOYER_PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
ENTRYPOINT_ADDRESS="${ENTRYPOINT_ADDRESS:-0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789}"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}    EIP-4337 SimpleAccount Deployment (Forge)${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════${NC}"
echo ""

# Check if forge is installed
if ! command -v forge &> /dev/null; then
    echo -e "${RED}❌ Error: forge (foundry) is not installed${NC}"
    echo -e "${YELLOW}Install: curl -L https://foundry.paradigm.xyz | bash && foundryup${NC}"
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

# Create a combined contract file
echo -e "${YELLOW}[1/3] Preparing contracts...${NC}"

cat > CombinedContracts.sol << 'COMBINED_EOF'
// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.23;

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

// IEntryPoint interface
interface IEntryPoint {
    function balanceOf(address account) external view returns (uint256);
    function depositTo(address account) external payable;
    function withdrawTo(address payable withdrawAddress, uint256 withdrawAmount) external;
    function getNonce(address sender, uint192 key) external view returns (uint256 nonce);
    function handleOps(UserOperation[] calldata ops, address payable beneficiary) external;
}

// SimpleAccount
contract SimpleAccount {
    IEntryPoint private immutable _entryPoint;
    address public owner;

    event SimpleAccountInitialized(IEntryPoint indexed entryPoint, address indexed owner);

    modifier onlyOwner() {
        _onlyOwner();
        _;
    }

    function _onlyOwner() internal view {
        require(msg.sender == owner || msg.sender == address(this), "only owner");
    }

    modifier onlyEntryPoint() {
        require(msg.sender == address(_entryPoint), "only EntryPoint");
        _;
    }

    constructor(IEntryPoint anEntryPoint) {
        _entryPoint = anEntryPoint;
        _disableInitializers();
    }

    function _disableInitializers() internal {
        require(owner == address(0), "already initialized");
        owner = address(1);
    }

    function initialize(address anOwner) public {
        require(owner == address(0), "already initialized");
        owner = anOwner;
        emit SimpleAccountInitialized(_entryPoint, owner);
    }

    function execute(address dest, uint256 value, bytes calldata func) external onlyEntryPoint {
        _call(dest, value, func);
    }

    function executeBatch(address[] calldata dest, uint256[] calldata value, bytes[] calldata func) external onlyEntryPoint {
        require(dest.length == func.length && dest.length == value.length, "wrong array lengths");
        for (uint256 i = 0; i < dest.length; i++) {
            _call(dest[i], value[i], func[i]);
        }
    }

    function validateUserOp(
        UserOperation calldata userOp,
        bytes32 userOpHash,
        uint256 missingAccountFunds
    ) external onlyEntryPoint returns (uint256 validationData) {
        validationData = _validateSignature(userOp, userOpHash);
        _payPrefund(missingAccountFunds);
    }

    function _validateSignature(UserOperation calldata userOp, bytes32 userOpHash)
        internal virtual view returns (uint256 validationData) {
        bytes32 hash = userOpHash.toEthSignedMessageHash();
        if (owner != hash.recover(userOp.signature))
            return SIG_VALIDATION_FAILED;
        return 0;
    }

    function _payPrefund(uint256 missingAccountFunds) internal {
        if (missingAccountFunds != 0) {
            (bool success,) = payable(msg.sender).call{value: missingAccountFunds, gas: type(uint256).max}("");
            (success);
        }
    }

    function _call(address target, uint256 value, bytes memory data) internal {
        (bool success, bytes memory result) = target.call{value: value}(data);
        if (!success) {
            assembly {
                revert(add(result, 32), mload(result))
            }
        }
    }

    function getDeposit() public view returns (uint256) {
        return _entryPoint.balanceOf(address(this));
    }

    function addDeposit() public payable {
        _entryPoint.depositTo{value: msg.value}(address(this));
    }

    function withdrawDepositTo(address payable withdrawAddress, uint256 amount) public onlyOwner {
        _entryPoint.withdrawTo(withdrawAddress, amount);
    }

    function entryPoint() public view returns (IEntryPoint) {
        return _entryPoint;
    }

    receive() external payable {}

    uint256 constant SIG_VALIDATION_FAILED = 1;
}

// ECDSA library
library ECDSA {
    function recover(bytes32 hash, bytes memory signature) internal pure returns (address) {
        if (signature.length != 65) {
            return address(0);
        }

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

    function toEthSignedMessageHash(bytes32 hash) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", hash));
    }
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
COMBINED_EOF

echo -e "${GREEN}✓ Contracts prepared${NC}"
echo ""

echo -e "${YELLOW}[2/3] Deploying SimpleAccountFactory...${NC}"

# Deploy using forge create
DEPLOY_OUTPUT=$(forge create CombinedContracts.sol:SimpleAccountFactory \
    --rpc-url $RPC_URL \
    --private-key $DEPLOYER_PRIVATE_KEY \
    --constructor-args $ENTRYPOINT_ADDRESS \
    --json 2>/dev/null || echo '{"deployedTo":""}')

FACTORY_ADDRESS=$(echo $DEPLOY_OUTPUT | jq -r '.deployedTo')

if [ -z "$FACTORY_ADDRESS" ] || [ "$FACTORY_ADDRESS" == "null" ] || [ "$FACTORY_ADDRESS" == "" ]; then
    echo -e "${RED}❌ Failed to deploy SimpleAccountFactory${NC}"
    echo -e "${YELLOW}Trying alternative deployment method...${NC}"
    
    # Alternative: compile and deploy manually
    forge build --contracts CombinedContracts.sol --force > /dev/null 2>&1
    
    FACTORY_BYTECODE=$(forge inspect CombinedContracts.sol:SimpleAccountFactory bytecode)
    CONSTRUCTOR_ARGS=$(cast abi-encode "constructor(address)" $ENTRYPOINT_ADDRESS)
    DEPLOY_CODE="${FACTORY_BYTECODE}${CONSTRUCTOR_ARGS:2}"
    
    FACTORY_ADDRESS=$(cast send --private-key $DEPLOYER_PRIVATE_KEY \
        --rpc-url $RPC_URL \
        --create $DEPLOY_CODE \
        --json | jq -r '.contractAddress')
fi

if [ -z "$FACTORY_ADDRESS" ] || [ "$FACTORY_ADDRESS" == "null" ]; then
    echo -e "${RED}❌ Failed to deploy SimpleAccountFactory${NC}"
    rm -f CombinedContracts.sol
    exit 1
fi

echo -e "${GREEN}✓ SimpleAccountFactory deployed at: $FACTORY_ADDRESS${NC}"
echo ""

echo -e "${YELLOW}[3/3] Computing wallet address...${NC}"

# Example wallet address
OWNER_ADDRESS="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
SALT="0"

WALLET_ADDRESS=$(cast call $FACTORY_ADDRESS \
    "getAddress(address,uint256)" \
    $OWNER_ADDRESS \
    $SALT \
    --rpc-url $RPC_URL)

echo -e "${GREEN}✓ Wallet address: $WALLET_ADDRESS${NC}"
echo ""

# Clean up
rm -f CombinedContracts.sol

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
echo -e "${YELLOW}📄 Saved to: $DEPLOYMENT_FILE${NC}"
echo ""
echo -e "${YELLOW}🚀 Next steps:${NC}"
echo ""
echo -e "1. Fund the wallet:"
echo -e "   ${BLUE}cast send $WALLET_ADDRESS --value 1ether --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL${NC}"
echo ""
echo -e "2. Create UserOperation:"
echo -e "   ${BLUE}cd .. && cargo run --example create_userop -- \\${NC}"
echo -e "   ${BLUE}    --private-key $DEPLOYER_PRIVATE_KEY \\${NC}"
echo -e "   ${BLUE}    --sender $WALLET_ADDRESS \\${NC}"
echo -e "   ${BLUE}    --entry-point $ENTRYPOINT_ADDRESS \\${NC}"
echo -e "   ${BLUE}    --chain-id $(cast chain-id --rpc-url $RPC_URL)${NC}"
echo ""


