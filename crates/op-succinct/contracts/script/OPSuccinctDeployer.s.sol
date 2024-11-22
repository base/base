// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

contract OPSuccinctDeployer is Script, Utils {
    function run() public returns (address) {
        vm.startBroadcast();

        Config memory config = readJson("opsuccinctl2ooconfig.json");

        // This initializes the proxy
        OPSuccinctL2OutputOracle oracleImpl = new OPSuccinctL2OutputOracle();
        Proxy proxy = new Proxy(msg.sender);

        upgradeAndInitialize(address(oracleImpl), config, address(proxy), true);

        vm.stopBroadcast();

        return address(proxy);
    }
}
