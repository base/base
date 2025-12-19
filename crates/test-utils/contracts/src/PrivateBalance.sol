// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title PrivateBalance
/// @notice A simple contract with a private mapping for testing privacy layer.
/// @dev Uses a mapping(address => uint256) which is the pattern for ownership-based privacy.
contract PrivateBalance {
    // Slot 0: A simple public counter
    uint256 public publicCounter;

    // Slot 1: A mapping for private balances (will be configured as private)
    mapping(address => uint256) public balances;

    // Slot 2: Admin address
    address public admin;

    constructor() {
        admin = msg.sender;
        publicCounter = 42;
    }

    /// @notice Set balance for a user (only admin can call)
    function setBalance(address user, uint256 amount) external {
        require(msg.sender == admin, "only admin");
        balances[user] = amount;
    }

    /// @notice Get balance for a user
    function getBalance(address user) external view returns (uint256) {
        return balances[user];
    }

    /// @notice Increment the public counter
    function incrementPublic() external {
        publicCounter++;
    }

    /// @notice Get admin address
    function getAdmin() external view returns (address) {
        return admin;
    }
}
