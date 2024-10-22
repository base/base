// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {OPSuccinctUpgrader} from "../script/OPSuccinctUpgrader.s.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {Utils} from "./helpers/Utils.sol";

contract UpgradeTest is Test, Utils {
    function testFreshDeployment() public {
        vm.startBroadcast();

        bytes32 exampleOutputRoot = keccak256("output root");
        vm.warp(12345678);
        uint256 exampleTimestamp = block.timestamp - 1;

        Config memory config = readJson("opsuccinctl2ooconfig.json");
        // This is never called, so we just need to add some code to the address so the check passes.
        config.verifierGateway = address(new Proxy(address(this)));
        config.startingOutputRoot = exampleOutputRoot;
        config.startingTimestamp = exampleTimestamp;
        OPSuccinctL2OutputOracle l2oo = OPSuccinctL2OutputOracle(deployWithConfig(config));

        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).outputRoot, exampleOutputRoot);
        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).timestamp, exampleTimestamp);

        vm.stopBroadcast();
    }
}
