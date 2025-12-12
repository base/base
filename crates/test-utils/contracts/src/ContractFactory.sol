// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ContractFactory
/// @notice Factory contract for testing CREATE and CREATE2 deployment tracking
contract ContractFactory {
    event Created(address indexed deployed);

    /// @notice Deploy a contract using CREATE opcode
    /// @param bytecode The bytecode to deploy
    /// @return addr The deployed contract address
    function deployWithCreate(bytes memory bytecode) public returns (address addr) {
        assembly {
            addr := create(0, add(bytecode, 0x20), mload(bytecode))
        }
        require(addr != address(0), "CREATE failed");
        emit Created(addr);
    }

    /// @notice Deploy a contract using CREATE2 opcode
    /// @param bytecode The bytecode to deploy
    /// @param salt The salt for deterministic address calculation
    /// @return addr The deployed contract address
    function deployWithCreate2(bytes memory bytecode, bytes32 salt) public returns (address addr) {
        assembly {
            addr := create2(0, add(bytecode, 0x20), mload(bytecode), salt)
        }
        require(addr != address(0), "CREATE2 failed");
        emit Created(addr);
    }

    /// @notice Deploy a contract and immediately call it
    /// @param bytecode The bytecode to deploy
    /// @param callData The call data to execute on the deployed contract
    /// @return addr The deployed contract address
    /// @return result The result of the call
    function deployAndCall(
        bytes memory bytecode,
        bytes memory callData
    ) public returns (address addr, bytes memory result) {
        assembly {
            addr := create(0, add(bytecode, 0x20), mload(bytecode))
        }
        require(addr != address(0), "CREATE failed");
        (bool success, bytes memory data) = addr.call(callData);
        require(success, "Call failed");
        result = data;
        emit Created(addr);
    }
}

/// @title SimpleStorage
/// @notice Simple contract for testing deployment and storage operations
contract SimpleStorage {
    uint256 public value;

    function setValue(uint256 v) public {
        value = v;
    }

    function getValue() public view returns (uint256) {
        return value;
    }
}
