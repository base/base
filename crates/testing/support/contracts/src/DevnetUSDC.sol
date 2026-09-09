// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {MockERC20} from "solmate/test/utils/mocks/MockERC20.sol";

/// @notice Standard 6-decimal USDC-like token for local real-token load-test setup.
contract DevnetUSDC is MockERC20 {
    constructor() MockERC20("Devnet USDC", "USDC", 6) {}
}
