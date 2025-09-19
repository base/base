// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

// Libraries
import {GameType} from "src/dispute/lib/Types.sol";

// Interfaces
import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";

contract MockOptimismPortal2 {
    /// @notice The delay between when a dispute game is resolved and when a withdrawal proven against it may be
    ///         finalized.
    uint256 internal immutable DISPUTE_GAME_FINALITY_DELAY_SECONDS;

    /// @notice A mapping of dispute game addresses to whether or not they are blacklisted.
    mapping(IDisputeGame => bool) public disputeGameBlacklist;

    /// @notice The game type that the OptimismPortal consults for output proposals.
    GameType public respectedGameType;

    /// @notice The timestamp at which the respected game type was last updated.
    uint64 public respectedGameTypeUpdatedAt;

    constructor(GameType _initialRespectedGameType, uint256 _disputeGameFinalityDelaySeconds) {
        respectedGameType = _initialRespectedGameType;
        respectedGameTypeUpdatedAt = uint64(block.timestamp);

        DISPUTE_GAME_FINALITY_DELAY_SECONDS = _disputeGameFinalityDelaySeconds;
    }

    function blacklistDisputeGame(IDisputeGame _disputeGame) external {
        disputeGameBlacklist[_disputeGame] = true;
    }

    function setRespectedGameType(GameType _gameType) external {
        respectedGameType = _gameType;
        respectedGameTypeUpdatedAt = uint64(block.timestamp);
    }

    function disputeGameFinalityDelaySeconds() public view returns (uint256) {
        return DISPUTE_GAME_FINALITY_DELAY_SECONDS;
    }
}
