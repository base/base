// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../src/validity/OPSuccinctL2OutputOracle.sol";
import {OPSuccinctDisputeGame} from "../src/validity/OPSuccinctDisputeGame.sol";
import {OPSuccinctDisputeGameFactory} from "../src/validity/OPSuccinctDisputeGameFactory.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

contract OPSuccinctDFGDeployer is Script, Utils {
    function run() public returns (address) {
        vm.startBroadcast();

        OPSuccinctL2OutputOracle l2OutputOracleProxy = OPSuccinctL2OutputOracle(vm.envAddress("L2OO_ADDRESS"));

        l2OutputOracleProxy.addProposer(address(0));

        // Initialize the dispute game based on the existing L2OO_ADDRESS.
        OPSuccinctDisputeGame game = new OPSuccinctDisputeGame(address(l2OutputOracleProxy));
        OPSuccinctDisputeGameFactory gameFactory = new OPSuccinctDisputeGameFactory(msg.sender, address(game));

        vm.stopBroadcast();

        return address(gameFactory);
    }
}
