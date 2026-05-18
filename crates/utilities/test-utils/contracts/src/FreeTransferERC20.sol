// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {MockERC20} from "solmate/test/utils/mocks/MockERC20.sol";

/// @notice ERC20 with unrestricted transferFrom — no approval required.
/// @dev For load testing only. Any address can move tokens from any other address.
contract FreeTransferERC20 is MockERC20 {
    constructor(
        string memory _name,
        string memory _symbol,
        uint8 _decimals
    ) MockERC20(_name, _symbol, _decimals) {}

    function transferFrom(address from, address to, uint256 amount) public override returns (bool) {
        balanceOf[from] -= amount;

        unchecked {
            balanceOf[to] += amount;
        }

        emit Transfer(from, to, amount);

        return true;
    }
}
