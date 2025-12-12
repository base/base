// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @notice Minimal TransparentUpgradeableProxy implementation for testing eth_call with delegatecall.
/// @dev Follows the pattern used by USDC and other major tokens.
/// The admin can only call admin functions, all other calls are delegated to implementation.
contract TransparentProxy {
    /// @dev Storage slot for the admin address (EIP-1967).
    /// bytes32(uint256(keccak256("eip1967.proxy.admin")) - 1)
    bytes32 private constant ADMIN_SLOT = 0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103;

    /// @dev Storage slot for the implementation address (EIP-1967).
    /// bytes32(uint256(keccak256("eip1967.proxy.implementation")) - 1)
    bytes32 private constant IMPLEMENTATION_SLOT = 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc;

    /// @notice Emitted when the implementation is upgraded.
    event Upgraded(address indexed implementation);

    /// @notice Emitted when the admin is changed.
    event AdminChanged(address previousAdmin, address newAdmin);

    /// @notice Initializes the proxy with an implementation and admin.
    /// @param _implementation The initial implementation address.
    /// @param _admin The proxy admin address.
    constructor(address _implementation, address _admin) {
        _setImplementation(_implementation);
        _setAdmin(_admin);
    }

    /// @notice Fallback function that delegates calls to the implementation.
    /// Admin calls are handled directly, non-admin calls are delegated.
    fallback() external payable {
        if (msg.sender == _getAdmin()) {
            // Admin cannot call implementation through proxy (transparency)
            revert("TransparentProxy: admin cannot fallback to implementation");
        }
        _delegate(_getImplementation());
    }

    /// @notice Receive function to accept ETH.
    receive() external payable {
        if (msg.sender != _getAdmin()) {
            _delegate(_getImplementation());
        }
    }

    /// @notice Returns the current implementation address.
    /// @dev Only callable by the admin.
    function implementation() external view returns (address) {
        require(msg.sender == _getAdmin(), "TransparentProxy: caller is not admin");
        return _getImplementation();
    }

    /// @notice Returns the current admin address.
    /// @dev Only callable by the admin.
    function admin() external view returns (address) {
        require(msg.sender == _getAdmin(), "TransparentProxy: caller is not admin");
        return _getAdmin();
    }

    /// @notice Upgrades the implementation.
    /// @dev Only callable by the admin.
    /// @param newImplementation The new implementation address.
    function upgradeTo(address newImplementation) external {
        require(msg.sender == _getAdmin(), "TransparentProxy: caller is not admin");
        _setImplementation(newImplementation);
        emit Upgraded(newImplementation);
    }

    /// @notice Changes the admin.
    /// @dev Only callable by the current admin.
    /// @param newAdmin The new admin address.
    function changeAdmin(address newAdmin) external {
        require(msg.sender == _getAdmin(), "TransparentProxy: caller is not admin");
        emit AdminChanged(_getAdmin(), newAdmin);
        _setAdmin(newAdmin);
    }

    /// @dev Delegates the current call to the implementation.
    function _delegate(address impl) internal {
        assembly {
            // Copy calldata to memory
            calldatacopy(0, 0, calldatasize())

            // Delegatecall to implementation
            let result := delegatecall(gas(), impl, 0, calldatasize(), 0, 0)

            // Copy returndata to memory
            returndatacopy(0, 0, returndatasize())

            switch result
            case 0 {
                // Revert with returndata
                revert(0, returndatasize())
            }
            default {
                // Return with returndata
                return(0, returndatasize())
            }
        }
    }

    /// @dev Returns the current implementation from storage.
    function _getImplementation() internal view returns (address impl) {
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly {
            impl := sload(slot)
        }
    }

    /// @dev Sets the implementation in storage.
    function _setImplementation(address newImplementation) internal {
        require(newImplementation.code.length > 0, "TransparentProxy: implementation is not a contract");
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly {
            sstore(slot, newImplementation)
        }
    }

    /// @dev Returns the current admin from storage.
    function _getAdmin() internal view returns (address adm) {
        bytes32 slot = ADMIN_SLOT;
        assembly {
            adm := sload(slot)
        }
    }

    /// @dev Sets the admin in storage.
    function _setAdmin(address newAdmin) internal {
        require(newAdmin != address(0), "TransparentProxy: new admin is zero address");
        bytes32 slot = ADMIN_SLOT;
        assembly {
            sstore(slot, newAdmin)
        }
    }
}
