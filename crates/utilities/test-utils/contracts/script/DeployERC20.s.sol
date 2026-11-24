// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {MockERC20} from "solmate/test/utils/mocks/MockERC20.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";

contract DeployERC20Script is Script {
    function run() public {
        vm.startBroadcast();

        MockERC20 token = new MockERC20("Test Token", "TEST", 18);
        console.log("MockERC20 deployed at: ", address(token));

        TransparentUpgradeableProxy proxy = new TransparentUpgradeableProxy(
            address(token),
            msg.sender,
            ""
        );
        console.log("TransparentUpgradeableProxy deployed at: ", address(proxy));

        vm.stopBroadcast();
    }
}
