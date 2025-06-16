// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

// Libraries
import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {Claim, GameType, Hash, OutputRoot, Duration} from "src/dispute/lib/Types.sol";
import {LibString} from "@solady/utils/LibString.sol";

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
import {OPSuccinctFaultDisputeGame} from "../../src/fp/OPSuccinctFaultDisputeGame.sol";
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

        // Use provided OptimismPortal2 address if given, otherwise deploy MockOptimismPortal2.
        address payable portalAddress;
        if (vm.envOr("OPTIMISM_PORTAL2_ADDRESS", address(0)) != address(0)) {
            portalAddress = payable(vm.envAddress("OPTIMISM_PORTAL2_ADDRESS"));
            console.log("Using existing OptimismPortal2:", portalAddress);
        } else {
            MockOptimismPortal2 portal =
                new MockOptimismPortal2(gameType, vm.envUint("DISPUTE_GAME_FINALITY_DELAY_SECONDS"));
            portalAddress = payable(address(portal));
            console.log("Deployed MockOptimismPortal2:", portalAddress);
        }

        OutputRoot memory startingAnchorRoot = OutputRoot({
            root: Hash.wrap(vm.envBytes32("STARTING_ROOT")),
            l2BlockNumber: vm.envUint("STARTING_L2_BLOCK_NUMBER")
        });

        // Deploy the anchor state registry proxy.
        ERC1967Proxy registryProxy = new ERC1967Proxy(
            address(new AnchorStateRegistry()),
            abi.encodeCall(
                AnchorStateRegistry.initialize,
                (
                    ISuperchainConfig(address(new SuperchainConfig())),
                    IDisputeGameFactory(address(factory)),
                    IOptimismPortal2(portalAddress),
                    startingAnchorRoot
                )
            )
        );

        AnchorStateRegistry registry = AnchorStateRegistry(address(registryProxy));
        console.log("Anchor state registry:", address(registry));

        // Get fallback timeout from environment variable, default to 2 weeks (1209600 seconds)
        uint256 fallbackTimeout = vm.envOr("FALLBACK_TIMEOUT_FP_SECS", uint256(1209600));
        console.log("Permissionless fallback timeout (seconds):", fallbackTimeout);

        // Deploy the access manager contract.
        AccessManager accessManager = new AccessManager(fallbackTimeout);
        console.log("Access manager:", address(accessManager));

        // Configure access control based on `PERMISSIONLESS_MODE` flag.
        if (vm.envOr("PERMISSIONLESS_MODE", false)) {
            // Set to permissionless games (anyone can propose and challenge).
            accessManager.setProposer(address(0), true);
            accessManager.setChallenger(address(0), true);
            console.log("Access Manager configured for permissionless mode");
        } else {
            // Set proposers from comma-separated list.
            string memory proposersStr = vm.envOr("PROPOSER_ADDRESSES", string(""));
            if (bytes(proposersStr).length > 0) {
                string[] memory proposers = LibString.split(proposersStr, ",");
                for (uint256 i = 0; i < proposers.length; i++) {
                    address proposer = vm.parseAddress(proposers[i]);
                    if (proposer != address(0)) {
                        accessManager.setProposer(proposer, true);
                        console.log("Added proposer:", proposer);
                    }
                }
            }

            // Set challengers from comma-separated list.
            string memory challengersStr = vm.envOr("CHALLENGER_ADDRESSES", string(""));
            if (bytes(challengersStr).length > 0) {
                string[] memory challengers = LibString.split(challengersStr, ",");
                for (uint256 i = 0; i < challengers.length; i++) {
                    address challenger = vm.parseAddress(challengers[i]);
                    if (challenger != address(0)) {
                        accessManager.setChallenger(challenger, true);
                        console.log("Added challenger:", challenger);
                    }
                }
            }
        }

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
            vm.envOr("CHALLENGER_BOND_WEI", uint256(0.001 ether)),
            IAnchorStateRegistry(address(registry)),
            accessManager
        );

        // Set initial bond and implementation in factory.
        factory.setInitBond(gameType, vm.envOr("INITIAL_BOND_WEI", uint256(0.001 ether)));
        factory.setImplementation(gameType, IDisputeGame(address(gameImpl)));

        vm.stopBroadcast();

        // Log deployed addresses.
        console.log("Factory Proxy:", address(factoryProxy));
        console.log("Game Implementation:", address(gameImpl));
        console.log("SP1 Verifier:", sp1VerifierAddress);
    }
}
