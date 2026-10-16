use alloy_sol_types::sol;

sol! {
    /// Fault-proof portal reads.
    interface FaultProofPortal {
        /// Proof registered by the given submitter.
        function provenWithdrawals(bytes32 withdrawalHash, address submitter)
            external view returns (address disputeGameProxy, uint64 timestamp);
        /// Required proof age in seconds.
        function proofMaturityDelaySeconds() external view returns (uint256);
        /// Reverts unless the withdrawal can be finalized.
        function checkWithdrawal(bytes32 withdrawalHash, address submitter) external view;
    }

    /// Legacy portal reads.
    interface LegacyWithdrawalPortal {
        /// Stored proof and its oracle index.
        function provenWithdrawals(bytes32 withdrawalHash) external view returns (
            bytes32 outputRoot, uint128 timestamp, uint128 l2OutputIndex
        );
    }

    /// Legacy output oracle reads.
    interface WithdrawalOutputOracle {
        /// Latest proposed L2 block.
        function latestBlockNumber() external view returns (uint256);
        /// First output covering a block.
        function getL2OutputIndexAfter(uint256 blockNumber) external view returns (uint256);
        /// Commitment, publication time, and block at an index.
        function getL2Output(uint256 index) external view returns (
            bytes32 outputRoot, uint128 timestamp, uint128 l2BlockNumber
        );
        /// Required age in seconds.
        function FINALIZATION_PERIOD_SECONDS() external view returns (uint256);
    }

    /// Dispute game resolution reads.
    interface WithdrawalDisputeGame {
        /// Zero while in progress, one for challenger victory, two for defender victory.
        function status() external view returns (uint8);
        /// L1 timestamp of resolution, or zero while in progress.
        function resolvedAt() external view returns (uint64);
        /// Encoded game parameters beginning with the L2 block number.
        function extraData() external view returns (bytes);
        /// Output root committed to by the game.
        function rootClaim() external view returns (bytes32);
    }
}
