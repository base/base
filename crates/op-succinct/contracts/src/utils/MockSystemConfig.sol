// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

import {ISuperchainConfig} from "interfaces/L1/ISuperchainConfig.sol";
import {SuperchainConfig} from "src/L1/SuperchainConfig.sol";

/// @notice Minimal mock SystemConfig for testing AnchorStateRegistry with v5.0.0
contract MockSystemConfig {
    ISuperchainConfig private _superchainConfig;
    address private _guardian;

    constructor(address guardian_) {
        _superchainConfig = ISuperchainConfig(address(new SuperchainConfig()));
        _guardian = guardian_;
    }

    /// @notice Returns whether the system is paused.
    function paused() external view returns (bool) {
        return _superchainConfig.paused();
    }

    /// @notice Returns the SuperchainConfig contract.
    function superchainConfig() external view returns (ISuperchainConfig) {
        return _superchainConfig;
    }

    /// @notice Returns the guardian address.
    function guardian() external view returns (address) {
        return _guardian;
    }
}
