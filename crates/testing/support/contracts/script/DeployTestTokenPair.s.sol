// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {FreeTransferERC20} from "../src/FreeTransferERC20.sol";

contract DeployTestTokenPair is Script {
    function run() public {
        vm.startBroadcast();

        FreeTransferERC20 tokenA = new FreeTransferERC20("Load Test Token A", "LTTA", 18);
        FreeTransferERC20 tokenB = new FreeTransferERC20("Load Test Token B", "LTTB", 18);

        tokenA.mint(msg.sender, 1_000_000_000 ether);
        tokenB.mint(msg.sender, 1_000_000_000 ether);

        console.log("Token A:", address(tokenA));
        console.log("Token B:", address(tokenB));

        vm.stopBroadcast();
    }
}
