// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title Minimal7702Account
/// @notice A minimal smart contract account for EIP-7702 delegation testing.
/// @dev When an EOA delegates to this contract via EIP-7702, calls to the EOA
///      will execute this contract's code in the context of the EOA's storage.
contract Minimal7702Account {
    /// @notice Emitted when execute is called
    event Executed(address indexed target, uint256 value, bytes data, bool success);

    /// @notice Emitted when a batch execute is called
    event BatchExecuted(uint256 count);

    /// @notice Counter for testing - stored in the delegating EOA's storage
    uint256 public counter;

    /// @notice Increment the counter
    function increment() external {
        counter++;
    }

    /// @notice Execute a call to another contract
    /// @param target The address to call
    /// @param value The ETH value to send
    /// @param data The calldata
    /// @return success Whether the call succeeded
    /// @return result The return data
    function execute(
        address target,
        uint256 value,
        bytes calldata data
    ) external payable returns (bool success, bytes memory result) {
        (success, result) = target.call{value: value}(data);
        emit Executed(target, value, data, success);
    }

    /// @notice Execute multiple calls in a single transaction
    /// @param targets Array of addresses to call
    /// @param values Array of ETH values to send
    /// @param datas Array of calldatas
    /// @return results Array of return data from each call
    function batchExecute(
        address[] calldata targets,
        uint256[] calldata values,
        bytes[] calldata datas
    ) external payable returns (bytes[] memory results) {
        require(
            targets.length == values.length && values.length == datas.length,
            "Length mismatch"
        );

        results = new bytes[](targets.length);
        for (uint256 i = 0; i < targets.length; i++) {
            (bool success, bytes memory result) = targets[i].call{value: values[i]}(datas[i]);
            require(success, "Call failed");
            results[i] = result;
        }

        emit BatchExecuted(targets.length);
    }

    /// @notice Receive ETH
    receive() external payable {}
}
