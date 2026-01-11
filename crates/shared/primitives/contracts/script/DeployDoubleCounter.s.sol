// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {DoubleCounter} from "../src/DoubleCounter.sol";

contract DeployDoubleCounterScript is Script {
    function run() public {
        vm.startBroadcast();
        DoubleCounter counter = new DoubleCounter();
        console.log("DoubleCounter deployed at: ", address(counter));
        vm.stopBroadcast();
    }
}
