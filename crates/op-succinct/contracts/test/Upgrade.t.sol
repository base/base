// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {ZKUpgrader} from "../script/ZKUpgrader.s.sol";
import {ZKL2OutputOracle} from "../src/ZKL2OutputOracle.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {Utils} from "./helpers/Utils.sol";

contract UpgradeTest is Test, Utils {
    function testReadJsonSucceeds() public {
        Config memory config = readJson("zkconfig.json");
        assertEq(config.l2BlockTime, 2);
        assertEq(config.proposer, address(0));
    }

    function testFreshDeployment() public {
        bytes32 exampleOutputRoot = keccak256("output root");
        vm.warp(12345678);
        uint256 exampleTimestamp = block.timestamp - 1;

        Config memory config = readJson("zkconfig.json");
        // This is never called, so we just need to add some code to the address so the check passes.
        config.verifierGateway = address(new Proxy(address(this)));
        ZKL2OutputOracle l2oo = ZKL2OutputOracle(deployWithConfig(config, exampleOutputRoot, exampleTimestamp));

        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).outputRoot, exampleOutputRoot);
        assertEq(l2oo.getL2Output(l2oo.latestOutputIndex()).timestamp, exampleTimestamp);
    }

    function testHexString() public {
        assertEq(createHexString(0), "0x0");
        assertEq(createHexString(1), "0x1");
        assertEq(createHexString(15), "0xf");
        assertEq(createHexString(16), "0x10");
        assertEq(createHexString(256), "0x100");
    }
}
