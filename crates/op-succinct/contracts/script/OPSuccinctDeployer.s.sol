// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";

contract OPSuccinctDeployer is Script, Utils {
    function run() public returns (address) {
        vm.startBroadcast();

        // Update the rollup config to match the current chain. If the starting block number is 0, the latest block number and starting output root will be fetched.
        updateRollupConfig();

        Config memory config = readJson("opsuccinctl2ooconfig.json");

        // This initializes the proxy.
        address l2OutputOracleProxy = address(new Proxy(msg.sender));

        address OPSuccinctL2OutputOracleImpl = address(new OPSuccinctL2OutputOracle());

        upgradeAndInitialize(OPSuccinctL2OutputOracleImpl, config, l2OutputOracleProxy, address(0));

        vm.stopBroadcast();

        return l2OutputOracleProxy;
    }
}
