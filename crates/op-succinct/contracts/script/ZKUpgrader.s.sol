// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import { Script } from "forge-std/Script.sol";
import { ZKL2OutputOracle } from "src/ZKL2OutputOracle.sol";
import { Utils } from "test/helpers/Utils.sol";


contract ZKUpgrader is Script, Utils {
    function run() public {
        Config memory config = readJson("zkconfig.json");

        vm.startBroadcast(vm.envUint("ADMIN_PK"));

        address zkL2OutputOracleImpl = address(new ZKL2OutputOracle());
        upgradeAndInitialize(zkL2OutputOracleImpl, config, address(0), bytes32(0), 0);

        vm.stopBroadcast();
    }
}
