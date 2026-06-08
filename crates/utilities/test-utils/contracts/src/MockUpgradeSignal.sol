// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Minimal L1 upgrade signal contract for local devnet tests.
contract MockUpgradeSignal {
    event TimestampSet(string indexed hardforkId, uint256 timestamp);
    event ProtocolVersionSet(string indexed hardforkId, uint256 protocolVersion);

    mapping(string hardforkId => uint256 timestamp) private timestamps;
    mapping(string hardforkId => uint256 protocolVersion) private protocolVersions;

    function getTimestamp(string calldata hardforkId) external view returns (uint256) {
        return timestamps[hardforkId];
    }

    function getProtocolVersion(string calldata hardforkId) external view returns (uint256) {
        return protocolVersions[hardforkId];
    }

    function setTimestamp(string calldata hardforkId, uint256 timestamp) external {
        timestamps[hardforkId] = timestamp;
        emit TimestampSet(hardforkId, timestamp);
    }

    function setProtocolVersion(string calldata hardforkId, uint256 protocolVersion) external {
        protocolVersions[hardforkId] = protocolVersion;
        emit ProtocolVersionSet(hardforkId, protocolVersion);
    }
}
