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
