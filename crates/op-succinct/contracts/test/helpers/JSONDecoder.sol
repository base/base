// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract JSONDecoder {
    struct Config {
        bytes32 aggregationVkey;
        address challenger;
        uint256 fallbackProposalTimeout;
        uint256 finalizationPeriod;
        uint256 l2BlockTime;
        address opSuccinctL2OutputOracleImpl;
        address owner;
        address proposer;
        address proxyAdmin;
        bytes32 rangeVkeyCommitment;
        bytes32 rollupConfigHash;
        uint256 startingBlockNumber;
        bytes32 startingOutputRoot;
        uint256 startingTimestamp;
        uint256 submissionInterval;
        address verifier;
    }

    struct OutputAtBlock {
        L2BlockRef blockRef;
        bytes32 outputRoot;
        bytes32 stateRoot;
        SyncStatus syncStatus;
        bytes32 version;
        bytes32 withdrawalStorageRoot;
    }

    struct SyncStatus {
        L1BlockRef currentL1;
        L1BlockRef currentL1Finalized;
        L1BlockRef finalizedL1;
        L2BlockRef finalizedL2;
        L1BlockRef headL1;
        L2BlockRef pendingSafeL2;
        L1BlockRef safeL1;
        L2BlockRef safeL2;
        L2BlockRef unsafeL2;
    }

    struct L1BlockRef {
        bytes32 hashOfBlock;
        uint256 number;
        bytes32 parentHash;
        uint256 timestamp;
    }

    struct L2BlockRef {
        bytes32 hash;
        L1Origin l1origin;
        uint256 number;
        bytes32 parentHash;
        uint256 sequenceNumber;
        uint256 timestamp;
    }

    struct L1Origin {
        bytes32 hash;
        uint256 blockNumber;
    }
}
