// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract JSONDecoder {
    struct L2OOConfig {
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

    struct SP1Config {
        address verifierAddress;
        bytes32 rollupConfigHash;
        bytes32 aggregationVkey;
        bytes32 rangeVkeyCommitment;
    }

    struct FDGConfig {
        bytes32 aggregationVkey;
        address[] challengerAddresses;
        uint256 challengerBondWei;
        uint256 disputeGameFinalityDelaySeconds;
        uint256 fallbackTimeoutFpSecs;
        uint32 gameType;
        uint256 initialBondWei;
        uint256 maxChallengeDuration;
        uint256 maxProveDuration;
        address optimismPortal2Address;
        bool permissionlessMode;
        address[] proposerAddresses;
        bytes32 rangeVkeyCommitment;
        bytes32 rollupConfigHash;
        uint256 startingL2BlockNumber;
        bytes32 startingRoot;
        bool useSp1MockVerifier;
        address verifierAddress;
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
