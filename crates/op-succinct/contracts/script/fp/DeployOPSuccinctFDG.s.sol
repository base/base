// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

// Libraries
import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {Claim, GameType, Hash, OutputRoot, Duration} from "src/dispute/lib/Types.sol";

// Interfaces
import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";
import {IDisputeGameFactory} from "interfaces/dispute/IDisputeGameFactory.sol";
import {ISP1Verifier} from "@sp1-contracts/src/ISP1Verifier.sol";
import {IAnchorStateRegistry} from "interfaces/dispute/IAnchorStateRegistry.sol";
import {ISuperchainConfig} from "interfaces/L1/ISuperchainConfig.sol";
import {IOptimismPortal2} from "interfaces/L1/IOptimismPortal2.sol";

// Contracts
import {AnchorStateRegistry} from "src/dispute/AnchorStateRegistry.sol";
import {AccessManager} from "../../src/fp/AccessManager.sol";
import {SuperchainConfig} from "src/L1/SuperchainConfig.sol";
import {DisputeGameFactory} from "src/dispute/DisputeGameFactory.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {OPSuccinctFaultDisputeGame} from "src/fp/OPSuccinctFaultDisputeGame.sol";
import {SP1MockVerifier} from "@sp1-contracts/src/SP1MockVerifier.sol";

// Utils
import {MockOptimismPortal2} from "../../utils/MockOptimismPortal2.sol";

contract DeployOPSuccinctFDG is Script {
    function run() public {
        vm.startBroadcast();

        // Deploy factory proxy.
        ERC1967Proxy factoryProxy = new ERC1967Proxy(
            address(new DisputeGameFactory()),
            abi.encodeWithSelector(DisputeGameFactory.initialize.selector, msg.sender)
        );
        DisputeGameFactory factory = DisputeGameFactory(address(factoryProxy));

        GameType gameType = GameType.wrap(uint32(vm.envUint("GAME_TYPE")));

        // TODO(fakedev9999): Use real OptimismPortal2.
        MockOptimismPortal2 portal =
            new MockOptimismPortal2(gameType, vm.envUint("DISPUTE_GAME_FINALITY_DELAY_SECONDS"));
        console.log("OptimismPortal2:", address(portal));

        OutputRoot memory startingAnchorRoot = OutputRoot({root: Hash.wrap(keccak256("genesis")), l2BlockNumber: 0});

        // Deploy the anchor state registry proxy.
        ERC1967Proxy registryProxy = new ERC1967Proxy(
            address(new AnchorStateRegistry()),
            abi.encodeCall(
                AnchorStateRegistry.initialize,
                (
                    ISuperchainConfig(address(new SuperchainConfig())),
                    IDisputeGameFactory(address(factory)),
                    IOptimismPortal2(payable(address(portal))),
                    startingAnchorRoot
                )
            )
        );

        AnchorStateRegistry registry = AnchorStateRegistry(address(registryProxy));
        console.log("Anchor state registry:", address(registry));
        // Deploy the access manager contract.
        AccessManager accessManager = new AccessManager();
        console.log("Access manager:", address(accessManager));

        // Set to permissionless games.
        // TODO(fakedev9999): Allow custom config with env vars.
        accessManager.setProposer(address(0), true);
        accessManager.setChallenger(address(0), true);

        // Config values dependent on the `USE_SP1_MOCK_VERIFIER` flag.
        address sp1VerifierAddress;
        bytes32 rollupConfigHash;
        bytes32 aggregationVkey;
        bytes32 rangeVkeyCommitment;

        // Get or deploy SP1 verifier based on environment variable.
        if (vm.envOr("USE_SP1_MOCK_VERIFIER", false)) {
            // Deploy mock verifier for testing.
            SP1MockVerifier sp1Verifier = new SP1MockVerifier();
            sp1VerifierAddress = address(sp1Verifier);
            console.log("Using SP1 Mock Verifier:", address(sp1Verifier));

            rollupConfigHash = bytes32(0);
            aggregationVkey = bytes32(0);
            rangeVkeyCommitment = bytes32(0);
        } else {
            // Use provided verifier address for production.
            sp1VerifierAddress = vm.envAddress("VERIFIER_ADDRESS");
            console.log("Using SP1 Verifier Gateway:", sp1VerifierAddress);

            rollupConfigHash = vm.envBytes32("ROLLUP_CONFIG_HASH");
            aggregationVkey = vm.envBytes32("AGGREGATION_VKEY");
            rangeVkeyCommitment = vm.envBytes32("RANGE_VKEY_COMMITMENT");
        }

        OPSuccinctFaultDisputeGame gameImpl = new OPSuccinctFaultDisputeGame(
            Duration.wrap(uint64(vm.envUint("MAX_CHALLENGE_DURATION"))),
            Duration.wrap(uint64(vm.envUint("MAX_PROVE_DURATION"))),
            IDisputeGameFactory(address(factory)),
            ISP1Verifier(sp1VerifierAddress),
            rollupConfigHash,
            aggregationVkey,
            rangeVkeyCommitment,
            vm.envOr("PROOF_REWARD", uint256(0.01 ether)),
            IAnchorStateRegistry(address(registry)),
            accessManager
        );

        // Set initial bond and implementation in factory.
        factory.setInitBond(gameType, vm.envOr("INITIAL_BOND", uint256(0.01 ether)));
        factory.setImplementation(gameType, IDisputeGame(address(gameImpl)));

        vm.stopBroadcast();

        // Log deployed addresses.
        console.log("Factory Proxy:", address(factoryProxy));
        console.log("Game Implementation:", address(gameImpl));
        console.log("SP1 Verifier:", sp1VerifierAddress);
    }
}
