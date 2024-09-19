// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {ZKL2OutputOracle} from "../src/ZKL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";

contract ZKUpgrader is Script, Utils {
    function run() public {
        // Update the rollup config to match the current chain. If the starting block number is 0, the latest block number and starting output root will be fetched.
        updateRollupConfig();

        Config memory config = readJson("zkl2ooconfig.json");

        vm.startBroadcast(vm.envUint("ADMIN_PK"));

        address zkL2OutputOracleImpl = address(new ZKL2OutputOracle());
        upgradeAndInitialize(zkL2OutputOracleImpl, config, address(0));

        vm.stopBroadcast();
    }
}
