// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {ZKL2OutputOracle} from "../src/ZKL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";

contract ZKDeployer is Script, Utils {
    function run() public returns (address) {
        vm.startBroadcast();

        Config memory config = readJsonWithRPCFromEnv("zkconfig.json");
        // Note: The owner of the proxy shouldn't be the msg.sender.
        config.l2OutputOracleProxy = address(new Proxy(config.owner));

        address zkL2OutputOracleImpl = address(new ZKL2OutputOracle());

        upgradeAndInitialize(zkL2OutputOracleImpl, config, address(0), bytes32(0), 0);

        vm.stopBroadcast();

        return config.l2OutputOracleProxy;
    }
}
