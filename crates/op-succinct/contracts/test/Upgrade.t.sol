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

    // TODO: These tests are failing because we need to figure out which chain they're associated with.
    // function testFetchOutputRoot() public {
    //     Config memory config = readJsonWithRPCFromEnv("zkconfig.json");
    //     (bytes32 root, uint256 ts) = fetchOutputRoot(config);
    //     assertEq(root, 0x6a2fb9128c8bc82eed49ee590fba3e975bd67fede20535d0d20b3000ea6d99b1);
    //     assertEq(ts, 1691802540);
    // }

    // function testUpgradeWorks() public {
    //     vm.createSelectFork("https://eth.llamarpc.com", 20528129);

    //     Config memory config = readJsonWithRPCFromEnv("zkconfig.json");
    //     config.l2OutputOracleProxy = 0xdfe97868233d1aa22e815a266982f2cf17685a27;

    //     address optimismProxyAdmin = 0x543bA4AADBAb8f9025686Bd03993043599c6fB04;
    //     address newImpl = address(new ZKL2OutputOracle());

    //     upgradeAndInitialize(newImpl, config, optimismProxyAdmin, bytes32(0), 0);

    //     ZKL2OutputOracle l2oo = ZKL2OutputOracle(config.l2OutputOracleProxy);
    //     assertEq(l2oo.owner(), address(0));
    //     assertEq(address(l2oo.verifierGateway()), 0x3B6041173B80E77f038f3F2C0f9744f04837185e);
    //     assertEq(l2oo.proposer(), address(0));
    // }

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
