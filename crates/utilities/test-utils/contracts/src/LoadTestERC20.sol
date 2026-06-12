// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// @notice Standard OpenZeppelin ERC20 with an open `mint`, deployed by the load tester to
///         generate ordinary `transfer(...)` traffic.
/// @dev For load testing only. The unrestricted `mint` lets the load tester distribute
///      balances to sender accounts during setup. No-argument constructor so the creation
///      bytecode needs no appended constructor args.
contract LoadTestERC20 is ERC20 {
    constructor() ERC20("Load Test Token", "LTT") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}
