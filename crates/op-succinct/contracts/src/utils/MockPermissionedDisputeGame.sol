// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";
import {GameStatus, GameType, Claim, Hash, Timestamp} from "src/dispute/lib/Types.sol";

/// @notice Minimal permissioned dispute game used exclusively for tests.
/// Exposes the legacy `claimData(uint256)` selector to emulate the ABI mismatch
/// encountered when scanning historical permissioned games.
contract MockPermissionedDisputeGame is IDisputeGame {
    bool private initialized;
    GameStatus private _status;
    Timestamp private _createdAt;
    Timestamp private _resolvedAt;

    function initialize() external payable override {
        require(!initialized, "already initialized");
        initialized = true;
        _createdAt = Timestamp.wrap(uint64(block.timestamp));
        _status = GameStatus.IN_PROGRESS;
    }

    function resolve() external override returns (GameStatus status_) {
        require(initialized, "not initialized");
        _status = GameStatus.DEFENDER_WINS;
        _resolvedAt = Timestamp.wrap(uint64(block.timestamp));
        status_ = _status;
    }

    function createdAt() external view returns (Timestamp) {
        return _createdAt;
    }

    function resolvedAt() external view returns (Timestamp) {
        return _resolvedAt;
    }

    function status() external view returns (GameStatus) {
        return _status;
    }

    function gameType() external pure returns (GameType gameType_) {
        gameType_ = GameType.wrap(1);
    }

    function gameCreator() external pure returns (address creator_) {
        creator_ = address(0);
    }

    function rootClaim() external pure returns (Claim rootClaim_) {
        rootClaim_ = Claim.wrap(bytes32(0));
    }

    function l1Head() external pure returns (Hash l1Head_) {
        l1Head_ = Hash.wrap(bytes32(0));
    }

    function l2BlockNumber() external pure returns (uint256 l2BlockNumber_) {
        l2BlockNumber_ = 0;
    }

    function extraData() external pure returns (bytes memory extraData_) {
        extraData_ = bytes("");
    }

    function gameData() external pure returns (GameType gameType_, Claim rootClaim_, bytes memory extraData_) {
        gameType_ = GameType.wrap(1);
        rootClaim_ = Claim.wrap(bytes32(0));
        extraData_ = bytes("");
    }

    function wasRespectedGameTypeWhenCreated() external pure returns (bool) {
        return false;
    }

    /// @notice Legacy games expose `claimData(uint256)` rather than zero-arg version.
    function claimData(uint256) external pure returns (bytes memory) {
        return bytes("");
    }
}
