// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {Types} from "@optimism/src/libraries/Types.sol";
import {Utils} from "../helpers/Utils.sol";

contract UpgradeTest is Test, Utils {
    function testFreshDeployment() public {
        vm.startBroadcast();

        bytes32 exampleOutputRoot = keccak256("output root");
        vm.warp(12345678);
        uint256 exampleTimestamp = block.timestamp - 1;

        L2OOConfig memory config = L2OOConfig({
            challenger: address(0),
            finalizationPeriod: 0,
            l2BlockTime: 10,
            owner: address(0xDEd0000E32f8F40414d3ab3a830f735a3553E18e),
            proposer: address(0xDEd0000E32f8F40414d3ab3a830f735a3553E18e),
            rollupConfigHash: bytes32(0x71241d0f92749d7365aaaf6a015de550816632a4e4e84e273f865f582e8190aa),
            startingBlockNumber: 132003,
            startingOutputRoot: bytes32(0x0cde567c088a52c8ddc32c76d954c6def0cf3418524e9d70bb05e713d9b07586),
            startingTimestamp: 1733438634,
            submissionInterval: 2,
            verifier: address(0x3B6041173B80E77f038f3F2C0f9744f04837185e),
            aggregationVkey: bytes32(0x00ea4171dbd0027768055bee7f6d64e17e9cec99b29aad5d18e5d804b967775b),
            rangeVkeyCommitment: bytes32(0x1a4ebe5c47d55436319c425951eb1a7e04f560945e29eb454215d30b30987bbb),
            proxyAdmin: address(0x0000000000000000000000000000000000000000),
            opSuccinctL2OutputOracleImpl: address(0x0000000000000000000000000000000000000000),
            fallbackProposalTimeout: 3600
        });

        // This is never called, so we just need to add some code to the address so the check passes.
        config.verifier = address(new Proxy(address(this)));
        config.startingOutputRoot = exampleOutputRoot;
        config.startingTimestamp = exampleTimestamp;
        OPSuccinctL2OutputOracle l2oo = OPSuccinctL2OutputOracle(deployWithConfig(config));

        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).outputRoot, exampleOutputRoot);
        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).timestamp, exampleTimestamp);

        vm.stopBroadcast();
    }

    /// NOTE: On the next upgrade, this test should additionally check that the non-genesis
    ///       opSuccinctConfigs are preserved.
    function testUpgradeExistingContract() public {
        // Fork Sepolia to test with real deployed contract
        vm.createSelectFork(vm.envString("L1_RPC"), 8621548);

        // This contract was deployed with release tag v2.3.0.
        // https://github.com/succinctlabs/op-succinct/tree/v2.3.0
        address existingL2OOProxy = 0xD810CbD4bD0BB01EcFD1064Aa4636436B96f8632;

        console.log("Testing Upgrade of Existing Contract");
        console.log("Existing contract address:", existingL2OOProxy);

        // Read current state before upgrade
        OPSuccinctL2OutputOracle existingContract = OPSuccinctL2OutputOracle(existingL2OOProxy);

        // Capture pre-upgrade state - only fields that are PERSISTENT between upgrades
        uint256 preLatestOutputIndex = existingContract.latestOutputIndex();
        uint256 preStartingBlockNumber = existingContract.startingBlockNumber();
        uint256 preStartingTimestamp = existingContract.startingTimestamp();
        uint256 preLatestBlockNumber = existingContract.latestBlockNumber();

        // Capture all existing L2 outputs to verify they persist
        Types.OutputProposal[] memory preOutputs = new Types.OutputProposal[](preLatestOutputIndex + 1);
        for (uint256 i = 0; i <= preLatestOutputIndex; i++) {
            preOutputs[i] = existingContract.getL2Output(i);
        }

        // Capture optimistic mode state (persistent)
        bool preOptimisticMode = existingContract.optimisticMode();

        // Capture approved proposers state (persistent)
        address approvedProposer = 0x4b713049Fc139df09A20F55f5b76c08184135DF8;
        address unapprovedProposer = 0x1234567890123456789012345678901234567890;
        bool preApprovedProposer = existingContract.approvedProposers(approvedProposer);
        bool preUnapprovedProposer = existingContract.approvedProposers(unapprovedProposer);

        // Create config for upgrade - these fields will be overwritten during initialization
        L2OOConfig memory config = L2OOConfig({
            challenger: address(0x1111111111111111111111111111111111111111), // Will be overwritten
            finalizationPeriod: 999999, // Will be overwritten
            l2BlockTime: 999999, // Will be overwritten
            owner: address(0x2222222222222222222222222222222222222222), // Will be overwritten
            proposer: address(0x3333333333333333333333333333333333333333), // Will be overwritten
            rollupConfigHash: bytes32(0x1111111111111111111111111111111111111111111111111111111111111111),
            startingBlockNumber: preStartingBlockNumber, // This is preserved if l2Outputs.length > 0
            startingOutputRoot: preOutputs[0].outputRoot, // This is preserved if l2Outputs.length > 0
            startingTimestamp: preStartingTimestamp, // This is preserved if l2Outputs.length > 0
            submissionInterval: 999999, // Will be overwritten
            verifier: address(0x1234567890123456789012345678901234567890), // Will be overwritten
            aggregationVkey: bytes32(0x2222222222222222222222222222222222222222222222222222222222222222),
            rangeVkeyCommitment: bytes32(0x3333333333333333333333333333333333333333333333333333333333333333),
            proxyAdmin: address(0x0000000000000000000000000000000000000000),
            opSuccinctL2OutputOracleImpl: address(0x0000000000000000000000000000000000000000),
            fallbackProposalTimeout: 999999 // Will be overwritten
        });

        // Deploy mock verifier contract
        vm.etch(
            config.verifier,
            hex"6080604052348015600f57600080fd5b506004361060285760003560e01c8063b8e2f40314602d575b600080fd5b60336035565b005b50565b"
        );

        vm.startPrank(0x4b713049Fc139df09A20F55f5b76c08184135DF8, 0x4b713049Fc139df09A20F55f5b76c08184135DF8);

        // Deploy new implementation
        config.opSuccinctL2OutputOracleImpl = address(new OPSuccinctL2OutputOracle());
        console.log("New implementation deployed at:", config.opSuccinctL2OutputOracleImpl);

        // Execute actual upgrade, and reinitialize the contract.
        console.log("Executing Upgrade");
        Proxy existingProxy = Proxy(payable(existingL2OOProxy));
        upgradeAndInitialize(config, address(existingProxy), true);

        vm.stopPrank();

        console.log("Post-Upgrade Verification");

        // Verify PERSISTENT state is preserved after upgrade
        // L2 outputs, optimistic mode, approved proposers, opSuccinctConfigs, historicBlockHashes
        assertEq(existingContract.latestOutputIndex(), preLatestOutputIndex, "Latest output index should be preserved");
        assertEq(
            existingContract.startingBlockNumber(), preStartingBlockNumber, "Starting block number should be preserved"
        );
        assertEq(existingContract.startingTimestamp(), preStartingTimestamp, "Starting timestamp should be preserved");
        assertEq(existingContract.latestBlockNumber(), preLatestBlockNumber, "Latest block number should be preserved");
        assertEq(existingContract.optimisticMode(), preOptimisticMode, "Optimistic mode should be preserved");

        // Verify approved proposers are preserved
        assertEq(
            existingContract.approvedProposers(approvedProposer),
            preApprovedProposer,
            "Approved proposer status should be preserved"
        );
        assertEq(
            existingContract.approvedProposers(unapprovedProposer),
            preUnapprovedProposer,
            "Unapproved proposer status should be preserved"
        );

        // Verify all L2 outputs are preserved
        for (uint256 i = 0; i <= preLatestOutputIndex; i++) {
            Types.OutputProposal memory postOutput = existingContract.getL2Output(i);
            assertEq(
                postOutput.outputRoot,
                preOutputs[i].outputRoot,
                string.concat("Output root at index ", vm.toString(i), " should be preserved")
            );
            assertEq(
                postOutput.timestamp,
                preOutputs[i].timestamp,
                string.concat("Timestamp at index ", vm.toString(i), " should be preserved")
            );
            assertEq(
                postOutput.l2BlockNumber,
                preOutputs[i].l2BlockNumber,
                string.concat("L2 block number at index ", vm.toString(i), " should be preserved")
            );
        }

        // Verify historicBlockHashes are preserved
        // https://sepolia.etherscan.io/tx/0x9fa14ae3a444a60b40bc43df19b72c69eb07d010a25816925a363560af9f4321
        // Manually checkpointed L1 block 8621538.
        assertEq(
            existingContract.historicBlockHashes(8621538),
            0x4bd41e855c3f4de0f0b26e43ea19ff5148e978918919b7ed30ea3479e7a696b6,
            "Historic block hash should be preserved"
        );
    }
}
