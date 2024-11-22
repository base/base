// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

contract OPSuccinctUpgrader is Script, Utils {
    function run() public {
        vm.startBroadcast();

        Config memory cfg = readJson("opsuccinctl2ooconfig.json");

        address l2OutputOracleProxy = vm.envAddress("L2OO_ADDRESS");
        bool executeUpgradeCall = vm.envOr("EXECUTE_UPGRADE_CALL", true);

        address OPSuccinctL2OutputOracleImpl = address(new OPSuccinctL2OutputOracle());

        upgradeAndInitialize(OPSuccinctL2OutputOracleImpl, cfg, l2OutputOracleProxy, executeUpgradeCall);

        vm.stopBroadcast();
    }
}
