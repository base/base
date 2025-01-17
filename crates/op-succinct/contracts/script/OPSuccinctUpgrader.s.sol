// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

contract OPSuccinctUpgrader is Script, Utils {
    function run() public {
        Config memory cfg = readJson("opsuccinctl2ooconfig.json");

        address l2OutputOracleProxy = vm.envAddress("L2OO_ADDRESS");
        bool executeUpgradeCall = vm.envOr("EXECUTE_UPGRADE_CALL", true);

        // Use implementation address from config
        address OPSuccinctL2OutputOracleImpl = cfg.opSuccinctL2OutputOracleImpl;
        address proxyAdmin = cfg.proxyAdmin;

        // optionally use a different key for deployment
        uint256 deployPk = vm.envOr("DEPLOY_PK", uint256(0));
        uint256 adminPk = vm.envOr("ADMIN_PK", uint256(0));

        // If deployPk is not set, use the default key.
        if (deployPk != uint256(0)) {
            vm.startBroadcast(deployPk);
        } else {
            vm.startBroadcast();
        }

        if (OPSuccinctL2OutputOracleImpl == address(0)) {
            console.log("Deploying new OPSuccinctL2OutputOracle impl");
            OPSuccinctL2OutputOracleImpl = address(new OPSuccinctL2OutputOracle());
        }

        vm.stopBroadcast();

        if (adminPk != uint256(0)) {
            vm.startBroadcast(adminPk);
        } else {
            vm.startBroadcast();
        }

        upgradeAndInitialize(cfg, l2OutputOracleProxy, executeUpgradeCall);

        vm.stopBroadcast();
    }
}
