// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

contract OPSuccinctDeployer is Script, Utils {
    function run() public returns (address) {
        uint256 deployPk = vm.envOr("DEPLOY_PK", uint256(0));
        uint256 adminPk = vm.envOr("ADMIN_PK", uint256(0));
        // If deployPk is not set, use the default key.
        if (deployPk != uint256(0)) {
            vm.startBroadcast(deployPk);
        } else {
            vm.startBroadcast();
        }

        string memory configPath =
            vm.envOr("OP_SUCCINCT_L2_OUTPUT_ORACLE_CONFIG_PATH", string("opsuccinctl2ooconfig.json"));
        L2OOConfig memory config = readL2OOJson(configPath);

        // Set the implementation address if it is not already set.
        if (config.opSuccinctL2OutputOracleImpl == address(0)) {
            console.log("Deploying new OPSuccinctL2OutputOracle impl");
            config.opSuccinctL2OutputOracleImpl = address(new OPSuccinctL2OutputOracle());
        }

        // If the Admin PK is set, use it as the owner of the proxy.
        address proxyOwner = adminPk != uint256(0) ? vm.addr(adminPk) : msg.sender;

        Proxy proxy = new Proxy(proxyOwner);

        vm.stopBroadcast();

        if (adminPk != uint256(0)) {
            vm.startBroadcast(adminPk);
        } else {
            vm.startBroadcast();
        }

        upgradeAndInitialize(config, address(proxy), true);

        vm.stopBroadcast();

        return address(proxy);
    }
}
