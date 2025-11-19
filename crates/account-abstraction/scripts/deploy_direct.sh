#!/bin/bash
# Deploy using forge create directly - simplest method
set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8545}"
DEPLOYER_PRIVATE_KEY="${DEPLOYER_PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
ENTRYPOINT_ADDRESS="${ENTRYPOINT_ADDRESS:-0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789}"

echo "═══════════════════════════════════════════════════════"
echo "    SimpleAccount Factory Deployment"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "RPC: $RPC_URL"
echo "EntryPoint: $ENTRYPOINT_ADDRESS"
echo ""

CONTRACTS_DIR="$(dirname "$0")/../contracts"
cd "$CONTRACTS_DIR"

# Create all-in-one contract file
cat > AllContracts.sol << 'EOF'
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
}

contract SimpleAccount {
    IEntryPoint private immutable _entryPoint;
    address public owner;

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
        
        // Recover signer from signature
        bytes32 ethSignedHash = keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", userOpHash));
        address signer = recoverSigner(ethSignedHash, userOp.signature);
        
        if (owner != signer) return 1; // SIG_VALIDATION_FAILED
        
        if (missingAccountFunds != 0) {
            (bool success,) = payable(msg.sender).call{value: missingAccountFunds}("");
            (success);
        }
        return 0;
    }

    function recoverSigner(bytes32 hash, bytes memory signature) internal pure returns (address) {
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

    receive() external payable {}
}

contract SimpleAccountFactory {
    IEntryPoint public immutable entryPoint;

    constructor(IEntryPoint _entryPoint) {
        entryPoint = _entryPoint;
    }

    function createAccount(address accountOwner, uint256 salt) public returns (SimpleAccount) {
        address addr = getAddress(accountOwner, salt);
        if (addr.code.length > 0) {
            return SimpleAccount(payable(addr));
        }
        SimpleAccount ret = SimpleAccount(payable(new SimpleAccount{salt: bytes32(salt)}(entryPoint)));
        ret.initialize(accountOwner);
        return ret;
    }

    function getAddress(address /* accountOwner */, uint256 salt) public view returns (address) {
        return address(uint160(uint256(keccak256(abi.encodePacked(
            bytes1(0xff),
            address(this),
            salt,
            keccak256(abi.encodePacked(type(SimpleAccount).creationCode, abi.encode(entryPoint)))
        )))));
    }
}
EOF

echo "Deploying with forge create..."
echo ""

# Deploy using forge create (does compile + deploy in one step)
DEPLOY_OUTPUT=$(forge create AllContracts.sol:SimpleAccountFactory \
    --rpc-url $RPC_URL \
    --private-key $DEPLOYER_PRIVATE_KEY \
    --constructor-args $ENTRYPOINT_ADDRESS \
    --optimize \
    --optimizer-runs 200 \
    2>&1)

echo "$DEPLOY_OUTPUT"
echo ""

# Extract address from output
FACTORY_ADDRESS=$(echo "$DEPLOY_OUTPUT" | grep "Deployed to:" | awk '{print $3}')

if [ -z "$FACTORY_ADDRESS" ]; then
    echo "❌ Could not find deployed address"
    rm -f AllContracts.sol
    exit 1
fi

echo "✅ Factory deployed: $FACTORY_ADDRESS"
echo ""

# Calculate example wallet address
OWNER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
SALT="0"

echo "Getting wallet address..."
WALLET=$(cast call $FACTORY_ADDRESS "getAddress(address,uint256)" $OWNER $SALT --rpc-url $RPC_URL)

echo "✅ Wallet: $WALLET"
echo ""

# Save deployment info
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
rm -f AllContracts.sol

echo ""
echo "═══════════════════════════════════════════════════════"
echo "✅ Success!"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "Next steps:"
echo ""
echo "1. Fund the wallet with ETH:"
echo "   cast send $WALLET --value 1ether --private-key $DEPLOYER_PRIVATE_KEY --rpc-url $RPC_URL"
echo ""
echo "2. Create a signed UserOperation:"
echo "   cd /Users/ericliu/Projects/node-reth/crates/account-abstraction"
echo "   cargo run --example create_userop -- \\"
echo "     --private-key $DEPLOYER_PRIVATE_KEY \\"
echo "     --sender $WALLET \\"
echo "     --entry-point $ENTRYPOINT_ADDRESS \\"
echo "     --chain-id $(cast chain-id --rpc-url $RPC_URL)"
echo ""


