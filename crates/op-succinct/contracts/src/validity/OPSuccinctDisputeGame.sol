// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {OPSuccinctL2OutputOracle} from "./OPSuccinctL2OutputOracle.sol";
import {Clone} from "@solady/utils/Clone.sol";
import {ISemver} from "interfaces/universal/ISemver.sol";
import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";
import {Claim, GameStatus, GameType, GameTypes, Hash, Timestamp} from "@optimism/src/dispute/lib/Types.sol";
import {GameNotInProgress, OutOfOrderResolution} from "@optimism/src/dispute/lib/Errors.sol";

contract OPSuccinctDisputeGame is ISemver, Clone, IDisputeGame {
    ////////////////////////////////////////////////////////////////
    //                         Events                             //
    ////////////////////////////////////////////////////////////////

    /// @notice The address of the L2 output oracle proxy contract.
    address internal immutable L2_OUTPUT_ORACLE;

    /// @notice The timestamp of the game's global creation.
    Timestamp public createdAt;

    /// @notice The timestamp of the game's global resolution.
    Timestamp public resolvedAt;

    /// @notice Returns the current status of the game.
    GameStatus public status;

    /// @notice A boolean for whether or not the game type was respected when the game was created.
    bool public wasRespectedGameTypeWhenCreated;

    /// @notice Semantic version.
    /// @custom:semver v3.0.0
    string public constant version = "v3.0.0";

    constructor(address _l2OutputOracle) {
        L2_OUTPUT_ORACLE = _l2OutputOracle;
    }

    ////////////////////////////////////////////////////////////
    //                    IDisputeGame impl                   //
    ////////////////////////////////////////////////////////////

    function initialize() external payable {
        createdAt = Timestamp.wrap(uint64(block.timestamp));
        status = GameStatus.IN_PROGRESS;
        wasRespectedGameTypeWhenCreated = true;

        OPSuccinctL2OutputOracle oracle = OPSuccinctL2OutputOracle(L2_OUTPUT_ORACLE);
        oracle.proposeL2Output(
            configName(), rootClaim().raw(), l2BlockNumber(), l1BlockNumber(), proof(), proverAddress()
        );

        this.resolve();
    }

    /// @notice Getter for the game type.
    /// @dev The reference impl should be entirely different depending on the type (fault, validity)
    ///      i.e. The game type should indicate the security model.
    /// @return gameType_ The type of proof system being used.
    function gameType() public pure returns (GameType) {
        return GameTypes.OP_SUCCINCT;
    }

    /// @notice Getter for the creator of the dispute game.
    /// @dev `clones-with-immutable-args` argument #1
    /// @return The creator of the dispute game.
    function gameCreator() public pure returns (address) {
        return _getArgAddress(0x00);
    }

    /// @notice Getter for the root claim.
    /// @dev `clones-with-immutable-args` argument #2
    /// @return The root claim of the DisputeGame.
    function rootClaim() public pure returns (Claim) {
        return Claim.wrap(_getArgBytes32(0x14));
    }

    /// @notice Getter for the parent hash of the L1 block when the dispute game was created.
    /// @dev `clones-with-immutable-args` argument #3
    /// @return The parent hash of the L1 block when the dispute game was created.
    function l1Head() public pure returns (Hash) {
        return Hash.wrap(_getArgBytes32(0x34));
    }

    /// @notice The l2BlockNumber of the disputed output root in the `L2OutputOracle`.
    function l2BlockNumber() public pure returns (uint256 l2BlockNumber_) {
        l2BlockNumber_ = _getArgUint256(0x54);
    }

    /// @notice The l2BlockNumber of the disputed output root in the `L2OutputOracle`.
    function l1BlockNumber() public pure returns (uint256 l1BlockNumber_) {
        l1BlockNumber_ = _getArgUint256(0x74);
    }

    /// @notice The prover address of the disputed output root in the `L2OutputOracle`.
    function proverAddress() public pure returns (address proverAddress_) {
        proverAddress_ = _getArgAddress(0x94);
    }

    /// @notice Getter for the config name.
    /// @return configName_ The config name to use for the L2OutputOracle.
    function configName() public pure returns (bytes32 configName_) {
        configName_ = _getArgBytes32(0xA8);
    }

    /// @notice The SP1 proof of the new output root. To be verified in the `L2OutputOracle`.
    function proof() public pure returns (bytes memory proof_) {
        uint256 len;
        assembly {
            // 0xC8 is the starting point of the proof in the calldata.
            // calldataload(sub(calldatasize(), 2)) loads the last 2 bytes of the calldata, which gives the length of the immutable args.
            // shr(240, calldataload(sub(calldatasize(), 2))) masks the last 30 bytes loaded in the previous step, so only the length of the immutable args is left.
            // sub(sub(...)) subtracts the length of the immutable args (2 bytes) and the starting point of the proof (0xC8).
            len := sub(sub(shr(240, calldataload(sub(calldatasize(), 2))), 2), 0xC8)
        }
        proof_ = _getArgBytes(0xC8, len);
    }

    /// @notice Getter for the extra data.
    /// @dev `clones-with-immutable-args` argument #4
    /// @return extraData_ Any extra data supplied to the dispute game contract by the creator.
    function extraData() public pure returns (bytes memory extraData_) {
        uint256 len;
        assembly {
            // 0x54 is the starting point of the extra data in the calldata.
            // calldataload(sub(calldatasize(), 2)) loads the last 2 bytes of the calldata, which gives the length of the immutable args.
            // shr(240, calldataload(sub(calldatasize(), 2))) masks the last 30 bytes loaded in the previous step, so only the length of the immutable args is left.
            // sub(sub(...)) subtracts the length of the immutable args (2 bytes) and the starting point of the extra data (0x54).
            len := sub(sub(shr(240, calldataload(sub(calldatasize(), 2))), 2), 0x54)
        }
        extraData_ = _getArgBytes(0x54, len);
    }

    /// @notice If all necessary information has been gathered, this function should mark the game
    ///         status as either `CHALLENGER_WINS` or `DEFENDER_WINS` and return the status of
    ///         the resolved game. It is at this stage that the bonds should be awarded to the
    ///         necessary parties.
    /// @dev May only be called if the `status` is `IN_PROGRESS`.
    /// @return status_ The status of the game after resolution.
    function resolve() external returns (GameStatus status_) {
        // INVARIANT: Resolution cannot occur unless the game is currently in progress.
        if (status != GameStatus.IN_PROGRESS) revert GameNotInProgress();

        resolvedAt = Timestamp.wrap(uint64(block.timestamp));
        status_ = GameStatus.DEFENDER_WINS;

        emit Resolved(status = status_);
    }

    /// @notice A compliant implementation of this interface should return the components of the
    ///         game UUID's preimage provided in the cwia payload. The preimage of the UUID is
    ///         constructed as `keccak256(gameType . rootClaim . extraData)` where `.` denotes
    ///         concatenation.
    /// @return gameType_ The type of proof system being used.
    /// @return rootClaim_ The root claim of the DisputeGame.
    /// @return extraData_ Any extra data supplied to the dispute game contract by the creator.
    function gameData() external pure returns (GameType gameType_, Claim rootClaim_, bytes memory extraData_) {
        gameType_ = gameType();
        rootClaim_ = rootClaim();
        extraData_ = extraData();
    }

    ////////////////////////////////////////////////////////////////
    //                     IMMUTABLE GETTERS                      //
    ////////////////////////////////////////////////////////////////

    /// @notice Getter for the L2OutputOracle.
    /// @return l2OutputOracle_ The address of the L2OutputOracle.
    function l2OutputOracle() external view returns (address l2OutputOracle_) {
        l2OutputOracle_ = L2_OUTPUT_ORACLE;
    }
}
