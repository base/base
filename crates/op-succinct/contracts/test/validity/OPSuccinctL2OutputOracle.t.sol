// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test} from "forge-std/Test.sol";
import {Utils} from "../helpers/Utils.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {SP1MockVerifier} from "@sp1-contracts/src/SP1MockVerifier.sol";
import {console} from "forge-std/console.sol";

contract OPSuccinctL2OutputOracleFallbackTest is Test, Utils {
    OPSuccinctL2OutputOracle l2oo;

    address approvedProposer = address(0x1234);
    address nonApprovedProposer = address(0x5678);
    address challenger = address(0x9ABC);
    address owner = address(0xDEF0);

    bytes32 aggregationVkey = keccak256("aggregation_vkey");
    bytes32 rangeVkeyCommitment = keccak256("range_vkey");
    bytes32 rollupConfigHash = keccak256("rollup_config");
    bytes32 startingOutputRoot = keccak256("starting_output");

    uint256 constant SUBMISSION_INTERVAL = 10;
    uint256 constant L2_BLOCK_TIME = 2;
    uint256 constant STARTING_BLOCK_NUMBER = 1000;
    uint256 constant FINALIZATION_PERIOD = 7 days;
    uint256 constant FALLBACK_TIMEOUT = 2 days;

    bytes32 genesisConfigName = bytes32(0);

    bytes proof = hex"";
    address proverAddress = address(0x7890);
    uint256 startingTimestamp = block.timestamp;

    function setUp() public {
        // Deploy L2OutputOracle using Utils helper function with custom parameters.
        address verifier = address(new SP1MockVerifier());
        OPSuccinctL2OutputOracle.InitParams memory initParams = createInitParamsWithFallback(
            verifier,
            approvedProposer,
            challenger,
            address(this),
            SUBMISSION_INTERVAL,
            L2_BLOCK_TIME,
            STARTING_BLOCK_NUMBER,
            FINALIZATION_PERIOD,
            FALLBACK_TIMEOUT
        );

        l2oo = deployL2OutputOracle(initParams);
        genesisConfigName = l2oo.GENESIS_CONFIG_NAME();
        // Set the timestamp to after the starting timestamp
        vm.warp(block.timestamp + 1000);
    }

    function testFallbackProposal_TimeoutElapsed_NonApprovedCanPropose() public {
        // Get the next block number to propose
        uint256 nextBlockNumber = l2oo.nextBlockNumber();
        bytes32 outputRoot = keccak256("test_output");

        // Warp time to exceed the fallback timeout
        uint256 lastProposalTime = l2oo.lastProposalTimestamp();
        vm.warp(lastProposalTime + FALLBACK_TIMEOUT + 1);

        // Checkpoint the current block hash
        uint256 currentL1Block = block.number;
        checkpointAndRoll(l2oo, currentL1Block);

        // Non-approved proposer should be able to propose after timeout
        vm.prank(nonApprovedProposer);
        l2oo.proposeL2Output(genesisConfigName, outputRoot, nextBlockNumber, currentL1Block, proof, proverAddress);

        // Verify the proposal was accepted
        assertEq(l2oo.latestBlockNumber(), nextBlockNumber);
        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).outputRoot, outputRoot);
    }

    function testFallbackProposal_TimeoutNotElapsed_NonApprovedCannotPropose() public {
        // Get the next block number to propose
        uint256 nextBlockNumber = l2oo.nextBlockNumber();
        bytes32 outputRoot = keccak256("test_output");

        // Don't warp time - fallback timeout has not elapsed
        uint256 lastProposalTime = l2oo.lastProposalTimestamp();
        vm.warp(lastProposalTime + FALLBACK_TIMEOUT - 1); // Just before timeout

        // Checkpoint the current block hash
        uint256 currentL1Block = block.number;
        checkpointAndRoll(l2oo, currentL1Block);

        // Non-approved proposer should NOT be able to propose before timeout
        vm.prank(nonApprovedProposer);
        vm.expectRevert("L2OutputOracle: only approved proposers can propose new outputs");
        l2oo.proposeL2Output(genesisConfigName, outputRoot, nextBlockNumber, currentL1Block, proof, proverAddress);
    }

    function testFallbackProposal_TimeoutNotElapsed_ApprovedCanStillPropose() public {
        // Get the next block number to propose
        uint256 nextBlockNumber = l2oo.nextBlockNumber();
        bytes32 outputRoot = keccak256("test_output");

        // Don't warp time - fallback timeout has not elapsed
        uint256 lastProposalTime = l2oo.lastProposalTimestamp();
        vm.warp(lastProposalTime + FALLBACK_TIMEOUT - 1); // Just before timeout

        // Checkpoint the current block hash
        uint256 currentL1Block = block.number;
        checkpointAndRoll(l2oo, currentL1Block);

        // Approved proposer should still be able to propose before timeout
        vm.prank(approvedProposer, approvedProposer);
        l2oo.proposeL2Output(genesisConfigName, outputRoot, nextBlockNumber, currentL1Block, proof, proverAddress);

        // Verify the proposal was accepted
        assertEq(l2oo.latestBlockNumber(), nextBlockNumber);
        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).outputRoot, outputRoot);
    }

    function testFallbackProposal_TimeoutElapsed_ApprovedCanStillPropose() public {
        // Get the next block number to propose
        uint256 nextBlockNumber = l2oo.nextBlockNumber();
        bytes32 outputRoot = keccak256("test_output");

        // Warp time to exceed the fallback timeout
        uint256 lastProposalTime = l2oo.lastProposalTimestamp();
        vm.warp(lastProposalTime + FALLBACK_TIMEOUT + 1);

        // Checkpoint the current block hash
        uint256 currentL1Block = block.number;
        checkpointAndRoll(l2oo, currentL1Block);

        // Approved proposer should still be able to propose after timeout
        vm.prank(approvedProposer);
        l2oo.proposeL2Output(genesisConfigName, outputRoot, nextBlockNumber, currentL1Block, proof, proverAddress);

        // Verify the proposal was accepted
        assertEq(l2oo.latestBlockNumber(), nextBlockNumber);
        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).outputRoot, outputRoot);
    }

    function testLastProposalTimestamp_InitialState() public view {
        // Initially, lastProposalTimestamp should return the starting timestamp
        assertEq(l2oo.lastProposalTimestamp(), startingTimestamp);
    }

    function testLastProposalTimestamp_AfterProposal() public {
        // Make a proposal
        uint256 nextBlockNumber = l2oo.nextBlockNumber();
        bytes32 outputRoot = keccak256("test_output");
        uint256 proposalTime = startingTimestamp + 5000;

        vm.warp(proposalTime);

        // Checkpoint the current block hash
        uint256 currentL1Block = block.number;
        checkpointAndRoll(l2oo, currentL1Block);

        vm.prank(approvedProposer, approvedProposer);
        l2oo.proposeL2Output(genesisConfigName, outputRoot, nextBlockNumber, currentL1Block, proof, proverAddress);

        // lastProposalTimestamp should now return the proposal time
        assertEq(l2oo.lastProposalTimestamp(), proposalTime);
    }

    function testFallbackProposalTimeout_Getter() public view {
        // Test that the getter returns the correct timeout value
        assertEq(l2oo.fallbackTimeout(), FALLBACK_TIMEOUT);
    }
}

contract OPSuccinctConfigManagementTest is Test, Utils {
    OPSuccinctL2OutputOracle l2oo;

    address owner = address(0x1234);
    address nonOwner = address(0x5678);

    bytes32 constant TEST_CONFIG_NAME = keccak256("test_config");
    bytes32 constant NEW_AGGREGATION_VKEY = keccak256("new_aggregation_key");
    bytes32 constant NEW_RANGE_VKEY = keccak256("new_range_key");
    bytes32 constant NEW_ROLLUP_CONFIG = keccak256("new_rollup_config");

    bytes32 genesisConfigName = bytes32(0);

    function setUp() public {
        // Deploy L2OutputOracle using Utils helper function
        address verifier = address(new SP1MockVerifier());
        OPSuccinctL2OutputOracle.InitParams memory initParams =
            createStandardInitParams(verifier, address(0x1111), address(0x2222), owner);

        l2oo = deployL2OutputOracle(initParams);
        genesisConfigName = l2oo.GENESIS_CONFIG_NAME();
    }

    function testUpdateOpSuccinctConfig_NewConfig() public {
        vm.prank(owner);

        l2oo.addOpSuccinctConfig(TEST_CONFIG_NAME, NEW_ROLLUP_CONFIG, NEW_AGGREGATION_VKEY, NEW_RANGE_VKEY);

        // Verify the configuration was stored
        (bytes32 aggVkey, bytes32 rangeVkey, bytes32 rollupConfig) = l2oo.opSuccinctConfigs(TEST_CONFIG_NAME);
        assertEq(aggVkey, NEW_AGGREGATION_VKEY);
        assertEq(rangeVkey, NEW_RANGE_VKEY);
        assertEq(rollupConfig, NEW_ROLLUP_CONFIG);
    }

    function testUpdateOpSuccinctConfig_DuplicateConfigName() public {
        // First create a test configuration
        vm.prank(owner);
        l2oo.addOpSuccinctConfig(TEST_CONFIG_NAME, NEW_ROLLUP_CONFIG, NEW_AGGREGATION_VKEY, NEW_RANGE_VKEY);

        // Try to add another configuration with the same name
        vm.prank(owner);
        vm.expectRevert("L2OutputOracle: config already exists");
        l2oo.addOpSuccinctConfig(
            TEST_CONFIG_NAME,
            keccak256("different_rollup_config"),
            keccak256("different_agg_key"),
            keccak256("different_range_key")
        );
    }

    function testDeleteOpSuccinctConfig_Success() public {
        // First create a test configuration
        vm.prank(owner);
        l2oo.addOpSuccinctConfig(TEST_CONFIG_NAME, NEW_ROLLUP_CONFIG, NEW_AGGREGATION_VKEY, NEW_RANGE_VKEY);

        // Now delete it
        vm.prank(owner);
        l2oo.deleteOpSuccinctConfig(TEST_CONFIG_NAME);

        // Verify it's deleted
        (,, bytes32 rollupConfig) = l2oo.opSuccinctConfigs(TEST_CONFIG_NAME);
        assertEq(rollupConfig, bytes32(0));
    }

    function testUpdateSubmissionInterval_CannotSetToZero() public {
        vm.prank(owner);
        vm.expectRevert("L2OutputOracle: submission interval must be greater than 0");
        l2oo.updateSubmissionInterval(0);
    }
}
