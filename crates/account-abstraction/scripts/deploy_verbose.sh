#!/bin/bash
# Deployment with verbose output to debug issues
set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8545}"
DEPLOYER_PRIVATE_KEY="${DEPLOYER_PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
ENTRYPOINT_ADDRESS="${ENTRYPOINT_ADDRESS:-0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789}"

echo "═══════════════════════════════════════════════════════"
echo "    SimpleAccount Factory Deployment (Verbose)"
echo "═══════════════════════════════════════════════════════"
echo ""

CONTRACTS_DIR="$(dirname "$0")/../contracts"
cd "$CONTRACTS_DIR"
echo "Working directory: $(pwd)"
echo ""

# Create a single flat file
cat > Flat.sol << 'EOF'
// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.23;

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

interface IEntryPoint {
    function balanceOf(address account) external view returns (uint256);
    function depositTo(address account) external payable;
    function withdrawTo(address payable withdrawAddress, uint256 withdrawAmount) external;
    function getNonce(address sender, uint192 key) external view returns (uint256 nonce);
    function handleOps(UserOperation[] calldata ops, address payable beneficiary) external;
}

library ECDSA {
    function recover(bytes32 hash, bytes memory signature) internal pure returns (address) {
        if (signature.length != 65) return address(0);
        bytes32 r; bytes32 s; uint8 v;
        assembly {
            r := mload(add(signature, 0x20))
            s := mload(add(signature, 0x40))
            v := byte(0, mload(add(signature, 0x60)))
        }
        if (uint256(s) > 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0) return address(0);
        return ecrecover(hash, v, r, s);
    }
    function toEthSignedMessageHash(bytes32 hash) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", hash));
    }
}

contract SimpleAccount {
    IEntryPoint private immutable _entryPoint;
    address public owner;
    uint256 constant SIG_VALIDATION_FAILED = 1;

    constructor(IEntryPoint anEntryPoint) {
        _entryPoint = anEntryPoint;
        owner = address(1);
    }

    function initialize(address anOwner) public {
        require(owner == address(0), "already initialized");
        owner = anOwner;
    }

    function execute(address dest, uint256 value, bytes calldata func) external {
        require(msg.sender == address(_entryPoint), "only EntryPoint");
        (bool success, bytes memory result) = dest.call{value: value}(func);
        if (!success) {
            assembly { revert(add(result, 32), mload(result)) }
        }
    }

    function validateUserOp(UserOperation calldata userOp, bytes32 userOpHash, uint256 missingAccountFunds) external returns (uint256) {
        require(msg.sender == address(_entryPoint), "only EntryPoint");
        bytes32 hash = ECDSA.toEthSignedMessageHash(userOpHash);
        if (owner != ECDSA.recover(hash, userOp.signature)) return SIG_VALIDATION_FAILED;
        if (missingAccountFunds != 0) {
            (bool success,) = payable(msg.sender).call{value: missingAccountFunds}("");
            (success);
        }
        return 0;
    }

    function entryPoint() public view returns (IEntryPoint) { return _entryPoint; }
    receive() external payable {}
}

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
            keccak256(abi.encodePacked(type(SimpleAccount).creationCode, abi.encode(entryPoint)))
        )))));
    }
}
EOF

echo "Created Flat.sol"
echo ""

echo "[1/3] Compiling with forge..."
echo "Running: forge build Flat.sol --force --optimize --optimizer-runs 200"
echo ""

# Compile with forge (show output)
forge build Flat.sol --force --optimize --optimizer-runs 200

if [ ! -d "out" ]; then
    echo "❌ Compilation failed - no out directory created"
    rm -f Flat.sol
    exit 1
fi

if [ ! -f "out/Flat.sol/SimpleAccountFactory.json" ]; then
    echo "❌ SimpleAccountFactory.json not found"
    ls -la out/Flat.sol/ || echo "out/Flat.sol/ directory doesn't exist"
    rm -rf Flat.sol out cache
    exit 1
fi

echo "✓ Compiled successfully"
echo ""

echo "[2/3] Preparing deployment..."

# Get bytecode
BYTECODE=$(jq -r '.bytecode.object' out/Flat.sol/SimpleAccountFactory.json)

if [ -z "$BYTECODE" ] || [ "$BYTECODE" == "null" ]; then
    echo "❌ Failed to extract bytecode"
    rm -rf Flat.sol out cache
    exit 1
fi

echo "✓ Bytecode extracted (${#BYTECODE} chars)"

# Encode constructor args
CONSTRUCTOR_ARGS=$(cast abi-encode "constructor(address)" $ENTRYPOINT_ADDRESS)
echo "✓ Constructor args: $CONSTRUCTOR_ARGS"

# Combine
DEPLOY_DATA="${BYTECODE}${CONSTRUCTOR_ARGS:2}"
echo "✓ Deploy data ready (${#DEPLOY_DATA} chars)"
echo ""

echo "[3/3] Deploying to $RPC_URL..."

# Deploy
RESULT=$(cast send --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL --create $DEPLOY_DATA --json)

echo "Deploy result: $RESULT"
echo ""

FACTORY_ADDRESS=$(echo $RESULT | jq -r '.contractAddress')

if [ -z "$FACTORY_ADDRESS" ] || [ "$FACTORY_ADDRESS" == "null" ]; then
    echo "❌ Deployment failed"
    rm -rf Flat.sol out cache
    exit 1
fi

echo "✓ Factory deployed: $FACTORY_ADDRESS"
echo ""

# Get wallet address
echo "Computing example wallet address..."
OWNER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
SALT="0"

WALLET=$(cast call $FACTORY_ADDRESS "getAddress(address,uint256)" $OWNER $SALT --rpc-url $RPC_URL)

echo "✓ Wallet: $WALLET"
echo ""

# Save
cat > ../deployment-info.json << DEPLOY_EOF
{
  "entryPoint": "$ENTRYPOINT_ADDRESS",
  "simpleAccountFactory": "$FACTORY_ADDRESS",
  "exampleWalletAddress": "$WALLET",
  "exampleOwner": "$OWNER",
  "exampleSalt": "$SALT",
  "rpcUrl": "$RPC_URL",
  "chainId": "$(cast chain-id --rpc-url $RPC_URL)"
}
DEPLOY_EOF

echo "Saved to deployment-info.json"

# Clean up
rm -rf Flat.sol out cache

echo ""
echo "═══════════════════════════════════════════════════════"
echo "✅ Done!"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "Next steps:"
echo "1. Fund: cast send $WALLET --value 1ether --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL"
echo "2. Create UserOp: cargo run --example create_userop -- --private-key $DEPLOYER_PRIVATE_KEY --sender $WALLET --entry-point $ENTRYPOINT_ADDRESS --chain-id $(cast chain-id --rpc-url $RPC_URL)"
echo ""


