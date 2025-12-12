// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title TransparentProxy
/// @notice A minimal transparent proxy for testing delegatecall patterns (USDC-style)
contract TransparentProxy {
    /// @notice Storage slot for the implementation address (EIP-1967)
    bytes32 private constant IMPLEMENTATION_SLOT = 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc;

    /// @notice Storage slot for the admin address (EIP-1967)
    bytes32 private constant ADMIN_SLOT = 0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103;

    constructor(address _implementation) {
        _setImplementation(_implementation);
        _setAdmin(msg.sender);
    }

    /// @notice Fallback function that delegates calls to the implementation
    fallback() external payable {
        _delegate(_getImplementation());
    }

    /// @notice Receive function for plain ETH transfers
    receive() external payable {
        _delegate(_getImplementation());
    }

    /// @notice Upgrade the implementation address (admin only)
    function upgradeTo(address newImplementation) external {
        require(msg.sender == _getAdmin(), "not admin");
        _setImplementation(newImplementation);
    }

    /// @notice Get the current implementation address
    function implementation() external view returns (address) {
        return _getImplementation();
    }

    /// @notice Get the current admin address
    function admin() external view returns (address) {
        return _getAdmin();
    }

    function _delegate(address impl) internal {
        assembly {
            // Copy msg.data
            calldatacopy(0, 0, calldatasize())

            // Delegatecall to implementation
            let result := delegatecall(gas(), impl, 0, calldatasize(), 0, 0)

            // Copy return data
            returndatacopy(0, 0, returndatasize())

            switch result
            case 0 {
                revert(0, returndatasize())
            }
            default {
                return(0, returndatasize())
            }
        }
    }

    function _getImplementation() internal view returns (address impl) {
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly {
            impl := sload(slot)
        }
    }

    function _setImplementation(address newImplementation) internal {
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly {
            sstore(slot, newImplementation)
        }
    }

    function _getAdmin() internal view returns (address adm) {
        bytes32 slot = ADMIN_SLOT;
        assembly {
            adm := sload(slot)
        }
    }

    function _setAdmin(address newAdmin) internal {
        bytes32 slot = ADMIN_SLOT;
        assembly {
            sstore(slot, newAdmin)
        }
    }
}
