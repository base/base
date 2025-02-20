// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import "forge-std/Script.sol";
import {SP1MockVerifier} from "@sp1-contracts/src/SP1MockVerifier.sol";

contract DeployMockVerifier is Script {
    function run() external returns (address) {
        vm.startBroadcast();

        SP1MockVerifier verifier = new SP1MockVerifier();

        vm.stopBroadcast();

        return address(verifier);
    }
}
