// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {ZKL2OutputOracle} from "../src/ZKL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";

contract ZKDeployer is Script, Utils {
    function run() public {
        vm.startBroadcast();

        Config memory config = readJsonWithRPCFromEnv("zkconfig.json");
        config.l2OutputOracleProxy = address(new Proxy(msg.sender));

        address zkL2OutputOracleImpl = address(new ZKL2OutputOracle());

        upgradeAndInitialize(zkL2OutputOracleImpl, config, address(0), bytes32(0), 0);

        vm.stopBroadcast();
    }
}
