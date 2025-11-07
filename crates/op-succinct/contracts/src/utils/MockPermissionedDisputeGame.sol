// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

import {Clone} from "@solady/utils/Clone.sol";
import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";
import {IDisputeGameFactory} from "interfaces/dispute/IDisputeGameFactory.sol";
import {GameStatus, GameType, Claim, Hash, Timestamp, OutputRoot} from "src/dispute/lib/Types.sol";

/// @notice Minimal permissioned dispute game used exclusively for tests.
/// Exposes the legacy `claimData(uint256)` selector to emulate the ABI mismatch
/// encountered when scanning historical permissioned games.
contract MockPermissionedDisputeGame is Clone, IDisputeGame {
    struct ClaimData {
        uint32 parentIndex;
        Claim claim;
    }

    IDisputeGameFactory internal immutable DISPUTE_GAME_FACTORY;
    bool private initialized;
    GameStatus private _status;
    Timestamp private _createdAt;
    Timestamp private _resolvedAt;
    ClaimData public claimData;
    OutputRoot private _startingOutputRoot;

    constructor(IDisputeGameFactory _disputeGameFactory) {
        DISPUTE_GAME_FACTORY = _disputeGameFactory;
    }

    function initialize() external payable override {
        require(!initialized, "already initialized");
        initialized = true;
        _createdAt = Timestamp.wrap(uint64(block.timestamp));
        _status = GameStatus.IN_PROGRESS;

        if (parentIndex() != type(uint32).max) {
            (,, IDisputeGame proxy) = DISPUTE_GAME_FACTORY.gameAtIndex(parentIndex());

            _startingOutputRoot = OutputRoot({
                l2BlockNumber: MockPermissionedDisputeGame(address(proxy)).l2BlockNumber(),
                root: Hash.wrap(MockPermissionedDisputeGame(address(proxy)).rootClaim().raw())
            });
        } else {
            _startingOutputRoot = OutputRoot({l2BlockNumber: 0, root: Hash.wrap(bytes32(0))});
        }

        claimData = ClaimData({parentIndex: parentIndex(), claim: rootClaim()});
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

    function gameType() public pure returns (GameType gameType_) {
        gameType_ = GameType.wrap(1);
    }

    function gameCreator() public pure returns (address creator_) {
        creator_ = _getArgAddress(0x00);
    }

    function rootClaim() public pure returns (Claim rootClaim_) {
        rootClaim_ = Claim.wrap(_getArgBytes32(0x14));
    }

    function l1Head() public pure returns (Hash l1Head_) {
        l1Head_ = Hash.wrap(_getArgBytes32(0x34));
    }

    function l2BlockNumber() external pure returns (uint256 l2BlockNumber_) {
        l2BlockNumber_ = _getArgUint256(0x54);
    }

    function parentIndex() public pure returns (uint32 parentIndex_) {
        parentIndex_ = _getArgUint32(0x74);
    }

    function extraData() public pure returns (bytes memory extraData_) {
        extraData_ = _getArgBytes(0x54, 0x24);
    }

    function gameData() external pure returns (GameType gameType_, Claim rootClaim_, bytes memory extraData_) {
        gameType_ = gameType();
        rootClaim_ = rootClaim();
        extraData_ = extraData();
    }

    function wasRespectedGameTypeWhenCreated() external pure returns (bool) {
        return true;
    }
}
