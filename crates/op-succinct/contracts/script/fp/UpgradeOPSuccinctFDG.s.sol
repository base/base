// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

// Libraries
import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {GameType, Duration} from "src/dispute/lib/Types.sol";

// Interfaces
import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";
import {IDisputeGameFactory} from "interfaces/dispute/IDisputeGameFactory.sol";
import {ISP1Verifier} from "@sp1-contracts/src/ISP1Verifier.sol";
import {IAnchorStateRegistry} from "interfaces/dispute/IAnchorStateRegistry.sol";

// Contracts
import {OPSuccinctFaultDisputeGame} from "../../src/fp/OPSuccinctFaultDisputeGame.sol";
import {DisputeGameFactory} from "src/dispute/DisputeGameFactory.sol";
import {AccessManager} from "../../src/fp/AccessManager.sol";

contract UpgradeOPSuccinctFDG is Script {
    function run() public {
        vm.startBroadcast();

        // Get the factory.
        address factoryAddress = vm.envAddress("FACTORY_ADDRESS");
        DisputeGameFactory factory = DisputeGameFactory(factoryAddress);

        // Get the game type.
        GameType gameType = GameType.wrap(uint32(vm.envUint("GAME_TYPE")));

        // Deploy new implementation.
        OPSuccinctFaultDisputeGame newImpl = new OPSuccinctFaultDisputeGame(
            Duration.wrap(uint64(vm.envUint("MAX_CHALLENGE_DURATION"))),
            Duration.wrap(uint64(vm.envUint("MAX_PROVE_DURATION"))),
            IDisputeGameFactory(factoryAddress),
            ISP1Verifier(vm.envAddress("VERIFIER_ADDRESS")),
            vm.envBytes32("ROLLUP_CONFIG_HASH"),
            vm.envBytes32("AGGREGATION_VKEY"),
            vm.envBytes32("RANGE_VKEY_COMMITMENT"),
            vm.envOr("CHALLENGER_BOND_WEI", uint256(0.001 ether)),
            IAnchorStateRegistry(vm.envAddress("ANCHOR_STATE_REGISTRY")),
            AccessManager(vm.envAddress("ACCESS_MANAGER"))
        );

        console.log("New OPSuccinctFaultDisputeGame implementation deployed at: ", address(newImpl));

        // Set the new implementation.
        factory.setImplementation(gameType, IDisputeGame(address(newImpl)));

        console.log("New implementation set in factory: ", address(factory.gameImpls(gameType)));

        vm.stopBroadcast();
    }
}
