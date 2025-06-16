// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {OPSuccinctUpgrader} from "../../script/validity/OPSuccinctUpgrader.s.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {Utils} from "../helpers/Utils.sol";

contract UpgradeTest is Test, Utils {
    function testFreshDeployment() public {
        vm.startBroadcast();

        bytes32 exampleOutputRoot = keccak256("output root");
        vm.warp(12345678);
        uint256 exampleTimestamp = block.timestamp - 1;

        Config memory config = Config({
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
            verifier: address(0x397A5f7f3dBd538f23DE225B51f532c34448dA9B),
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
}
