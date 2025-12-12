// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {ERC20} from "../lib/solmate/src/tokens/ERC20.sol";

/// @notice Simple ERC-20 token for testing eth_call functionality.
/// @dev Extends Solmate's ERC20 with public mint/burn functions for test setup.
contract TestToken is ERC20 {
    constructor(
        string memory _name,
        string memory _symbol,
        uint8 _decimals
    ) ERC20(_name, _symbol, _decimals) {}

    /// @notice Mints tokens to a specified address.
    /// @param to The address to receive the minted tokens.
    /// @param amount The amount of tokens to mint.
    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    /// @notice Burns tokens from a specified address.
    /// @param from The address to burn tokens from.
    /// @param amount The amount of tokens to burn.
    function burn(address from, uint256 amount) external {
        _burn(from, amount);
    }
}
