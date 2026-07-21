// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Minimal mutable `ProtocolVersions` upgrade schedule contract for local devnets and
/// system tests.
///
/// Implements the read interface consumed by node upgrade-signal readers
/// (`IProtocolVersions` in `base-upgrade-signal`): `getSchedule()` returns activation
/// timestamps ordered by ascending upgrade registration id (`0` = not scheduled), and
/// `minimumProtocolVersion()` returns the packed-semver minimum client version.
contract MockProtocolVersions {
    event ScheduleSet(uint64[] schedule);
    event MinimumProtocolVersionSet(uint256 minimumProtocolVersion);

    uint64[] private schedule;
    uint256 private minimumVersion;

    /// @notice Returns the activation timestamp for every registered upgrade, ordered by
    /// ascending upgrade id (`0` = not scheduled).
    function getSchedule() external view returns (uint64[] memory) {
        return schedule;
    }

    /// @notice Returns the minimum protocol version clients must run (packed semver).
    function minimumProtocolVersion() external view returns (uint256) {
        return minimumVersion;
    }

    /// @notice Replaces the id-ordered activation timestamp schedule.
    function setSchedule(uint64[] calldata newSchedule) external {
        schedule = newSchedule;
        emit ScheduleSet(newSchedule);
    }

    /// @notice Sets the minimum protocol version clients must run (packed semver).
    function setMinimumProtocolVersion(uint256 newMinimumProtocolVersion) external {
        minimumVersion = newMinimumProtocolVersion;
        emit MinimumProtocolVersionSet(newMinimumProtocolVersion);
    }
}
