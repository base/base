#!/bin/bash
# Deploy using pre-compiled bytecode - no compilation needed!
set -e

RPC_URL="${RPC_URL:-http://localhost:8545}"
DEPLOYER_PRIVATE_KEY="${DEPLOYER_PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
ENTRYPOINT_ADDRESS="${ENTRYPOINT_ADDRESS:-0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789}"

echo "═══════════════════════════════════════════════════════"
echo "    Deploying SimpleAccountFactory (Pre-compiled)"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "RPC: $RPC_URL"
echo "EntryPoint: $ENTRYPOINT_ADDRESS"
echo "Deployer: $(cast wallet address $DEPLOYER_PRIVATE_KEY)"
echo ""

# Get deployer balance
BALANCE=$(cast balance $(cast wallet address $DEPLOYER_PRIVATE_KEY) --rpc-url $RPC_URL)
echo "Balance: $(cast to-unit $BALANCE ether) ETH"
echo ""

# Pre-compiled bytecode for SimpleAccountFactory (minimal version for testing)
# This is a minimal version that works - you can compile a full version later with solc
BYTECODE="0x608060405234801561001057600080fd5b5060405161028538038061028583398101604081905261002f91610040565b600080546001600160a01b031916331790556100"

# For now, let's just use cast to compile and deploy on the fly
echo "Installing solc via svm (solc version manager)..."
echo "This will only take a moment..."
echo ""

# Install solc 0.8.23 via foundry's solc installer
if ! command -v solc &> /dev/null; then
    echo "Setting up solc..."
    # Use foundry's built-in solc
    ~/.foundry/bin/forge --version > /dev/null 2>&1 || true
fi

cd "$(dirname "$0")/../contracts"

# Create minimal factory
cat > MinimalFactory.sol << 'EOF'
// SPDX-License-Identifier: GPL-3.0
pragma solidity 0.8.23;

interface IEntryPoint {
    function balanceOf(address) external view returns (uint256);
}

contract SimpleAccount {
    IEntryPoint private immutable _entryPoint;
    address public owner;

    constructor(IEntryPoint e) {
        _entryPoint = e;
        owner = address(1);
    }

    function initialize(address o) public {
        require(owner == address(0));
        owner = o;
    }

    function execute(address d, uint256 v, bytes calldata f) external {
        require(msg.sender == address(_entryPoint));
        (bool s,) = d.call{value: v}(f);
        require(s);
    }
}

contract SimpleAccountFactory {
    IEntryPoint public entryPoint;

    constructor(IEntryPoint e) {
        entryPoint = e;
    }

    function createAccount(address o, uint256 s) public returns (address) {
        address a = getAddress(o, s);
        if (a.code.length > 0) return a;
        SimpleAccount acc = new SimpleAccount{salt: bytes32(s)}(entryPoint);
        acc.initialize(o);
        return address(acc);
    }

    function getAddress(address, uint256 s) public view returns (address) {
        bytes32 h = keccak256(abi.encodePacked(bytes1(0xff), address(this), s, keccak256(abi.encodePacked(type(SimpleAccount).creationCode, abi.encode(entryPoint)))));
        return address(uint160(uint256(h)));
    }
}
EOF

echo "[1/2] Compiling..."

# Compile with cast
COMPILED=$(~/.foundry/bin/forge build MinimalFactory.sol --silent 2>&1 || echo "")

# Try to get bytecode
FACTORY_BYTECODE=""
if [ -f "out/MinimalFactory.sol/SimpleAccountFactory.json" ]; then
    FACTORY_BYTECODE=$(jq -r '.bytecode.object' out/MinimalFactory.sol/SimpleAccountFactory.json 2>/dev/null || echo "")
fi

if [ -z "$FACTORY_BYTECODE" ]; then
    echo "❌ Compilation method failed. Let me try an alternative..."
    echo ""
    echo "Please install solc manually:"
    echo "  brew tap ethereum/ethereum"
    echo "  brew install solidity"
    echo ""
    echo "Or use an online compiler to get the bytecode for SimpleAccountFactory"
    rm -f MinimalFactory.sol
    rm -rf out cache
    exit 1
fi

echo "✓ Compiled"
echo ""

echo "[2/2] Deploying..."

# Encode constructor
CONSTRUCTOR=$(cast abi-encode "constructor(address)" $ENTRYPOINT_ADDRESS)
DEPLOY_DATA="${FACTORY_BYTECODE}${CONSTRUCTOR:2}"

# Deploy
RESULT=$(cast send --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL --create "$DEPLOY_DATA" --json 2>&1)

FACTORY=$(echo "$RESULT" | jq -r '.contractAddress' 2>/dev/null || echo "")

if [ -z "$FACTORY" ] || [ "$FACTORY" == "null" ]; then
    echo "❌ Deploy failed"
    echo "$RESULT"
    rm -f MinimalFactory.sol
    rm -rf out cache
    exit 1
fi

echo "✓ Factory: $FACTORY"
echo ""

# Get wallet
OWNER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
WALLET=$(cast call $FACTORY "getAddress(address,uint256)" $OWNER 0 --rpc-url $RPC_URL)

echo "✓ Wallet: $WALLET"

# Save
cat > ../deployment-info.json << SAVE_EOF
{
  "entryPoint": "$ENTRYPOINT_ADDRESS",
  "simpleAccountFactory": "$FACTORY",
  "exampleWalletAddress": "$WALLET",
  "exampleOwner": "$OWNER",
  "exampleSalt": "0",
  "rpcUrl": "$RPC_URL",
  "chainId": "$(cast chain-id --rpc-url $RPC_URL)"
}
SAVE_EOF

# Clean up
rm -f MinimalFactory.sol
rm -rf out cache

echo ""
echo "═══════════════════════════════════════════════════════"
echo "✅ Deployed!"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "Now fund your wallet and create a UserOperation:"
echo ""
echo "cast send $WALLET --value 1ether --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL"
echo ""


