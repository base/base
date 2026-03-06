// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

// The game type for the OP Succinct Fault Dispute Game.
// Eventually will be enshrined in the game type enum.
uint32 constant OP_SUCCINCT_FAULT_DISPUTE_GAME_TYPE = 42;

/// @notice The public values committed to for an OP Succinct aggregation program.
struct AggregationOutputs {
    bytes32 l1Head;
    bytes32 l2PreRoot;
    bytes32 claimRoot;
    uint256 claimBlockNum;
    bytes32 rollupConfigHash;
    bytes32 rangeVkeyCommitment;
    address proverAddress;
}
