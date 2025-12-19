// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {PrivacyEnabled} from "../base/PrivacyEnabled.sol";

/// @title PrivateERC20
/// @author Base Privacy Team
/// @notice ERC20 token with private balances and allowances
/// @dev Extends OpenZeppelin ERC20 with privacy layer integration
///
/// @dev How it works:
/// - Balances are stored in a mapping at slot 0 (OpenZeppelin ERC20 layout)
/// - Allowances are stored in a nested mapping at slot 1
/// - Each user owns their own balance data (MappingKey ownership)
/// - Each owner controls visibility of their allowances (OuterKey ownership)
///
/// @dev Storage layout (must match OpenZeppelin ERC20):
/// - Slot 0: mapping(address => uint256) _balances
/// - Slot 1: mapping(address => mapping(address => uint256)) _allowances
/// - Slot 2: uint256 _totalSupply
/// - Slot 3: string _name
/// - Slot 4: string _symbol
///
/// @dev Privacy features:
/// - Balances are hidden from RPC queries for non-owners
/// - Users can grant view access to specific addresses
/// - Allowances are hidden unless you're the owner
contract PrivateERC20 is ERC20, PrivacyEnabled {
    /*//////////////////////////////////////////////////////////////
                              CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @dev Storage slot for _balances mapping in OpenZeppelin ERC20
    uint256 private constant BALANCES_SLOT = 0;

    /// @dev Storage slot for _allowances mapping in OpenZeppelin ERC20
    uint256 private constant ALLOWANCES_SLOT = 1;

    /*//////////////////////////////////////////////////////////////
                             CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    /// @notice Deploy a new PrivateERC20 token
    /// @param name_ Token name
    /// @param symbol_ Token symbol
    constructor(
        string memory name_,
        string memory symbol_
    ) ERC20(name_, symbol_) {
        // Register private storage slots
        // Slot 0: _balances - each address owns their balance entry
        _registerPrivateMapping(BALANCES_SLOT);

        // Slot 1: _allowances - owner controls visibility of their allowances
        _registerPrivateAllowances(ALLOWANCES_SLOT);

        // Finalize registration (deployer becomes privacy admin)
        _finalizePrivacyRegistration();
    }

    /*//////////////////////////////////////////////////////////////
                          PRIVACY UTILITIES
    //////////////////////////////////////////////////////////////*/

    /// @notice Allow an address to view your balance
    /// @dev Grants READ access to the caller's balance slot
    /// @param viewer Address to grant view access to
    function grantBalanceView(address viewer) external {
        uint256 slot = _computeMappingSlot(msg.sender, BALANCES_SLOT);
        _grantReadAccess(slot, viewer);
    }

    /// @notice Revoke balance view access from an address
    /// @param viewer Address to revoke view access from
    function revokeBalanceView(address viewer) external {
        uint256 slot = _computeMappingSlot(msg.sender, BALANCES_SLOT);
        _revokeAccess(slot, viewer);
    }

    /// @notice Allow an address to view your allowances to a specific spender
    /// @dev Grants READ access to a specific (owner, spender) allowance slot
    /// @param spender The spender whose allowance entry to share
    /// @param viewer Address to grant view access to
    function grantAllowanceView(address spender, address viewer) external {
        uint256 slot = _computeNestedMappingSlot(msg.sender, spender, ALLOWANCES_SLOT);
        _grantReadAccess(slot, viewer);
    }

    /// @notice Revoke allowance view access from an address
    /// @param spender The spender whose allowance entry to hide
    /// @param viewer Address to revoke view access from
    function revokeAllowanceView(address spender, address viewer) external {
        uint256 slot = _computeNestedMappingSlot(msg.sender, spender, ALLOWANCES_SLOT);
        _revokeAccess(slot, viewer);
    }

    /*//////////////////////////////////////////////////////////////
                           MINTING (FOR TESTING)
    //////////////////////////////////////////////////////////////*/

    /// @notice Mint tokens to an address
    /// @dev Only for testing purposes - production tokens should have proper access control
    /// @param to Address to mint tokens to
    /// @param amount Amount of tokens to mint
    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    /// @notice Burn tokens from an address
    /// @dev Only for testing purposes
    /// @param from Address to burn tokens from
    /// @param amount Amount of tokens to burn
    function burn(address from, uint256 amount) external {
        _burn(from, amount);
    }
}
