// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

// Libraries
import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {stdJson} from "forge-std/StdJson.sol";
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
import {Utils} from "../../test/helpers/Utils.sol";
import {MockOptimismPortal2} from "../../src/utils/MockOptimismPortal2.sol";

contract DeployOPSuccinctFDG is Script, Utils {
    using stdJson for string;

    struct DeployedContracts {
        address factoryProxy;
        address gameImplementation;
        address sp1Verifier;
        address anchorStateRegistry;
        address accessManager;
        address optimismPortal2;
    }

    function run()
        public
        returns (
            address factoryProxy,
            address gameImplementation,
            address sp1Verifier,
            address anchorStateRegistry,
            address accessManager,
            address optimismPortal2
        )
    {
        vm.startBroadcast();

        // Load configuration
        string memory configPath =
            vm.envOr("OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH", string("opsuccinctfdgconfig.json"));
        FDGConfig memory config = readFDGJson(configPath);

        // Deploy contracts
        DeployedContracts memory deployedContracts = deployContracts(config);

        vm.stopBroadcast();

        return (
            deployedContracts.factoryProxy,
            deployedContracts.gameImplementation,
            deployedContracts.sp1Verifier,
            deployedContracts.anchorStateRegistry,
            deployedContracts.accessManager,
            deployedContracts.optimismPortal2
        );
    }

    function deployContracts(FDGConfig memory config) internal returns (DeployedContracts memory) {
        // Deploy factory proxy.
        ERC1967Proxy factoryProxy = new ERC1967Proxy(
            address(new DisputeGameFactory()),
            abi.encodeWithSelector(DisputeGameFactory.initialize.selector, msg.sender)
        );
        DisputeGameFactory factory = DisputeGameFactory(address(factoryProxy));

        GameType gameType = GameType.wrap(config.gameType);

        // Deploy MockOptimismPortal2 or get OptimismPortal2
        address payable portalAddress = deployOrGetOptimismPortal2(config, gameType);

        OutputRoot memory startingAnchorRoot =
            OutputRoot({root: Hash.wrap(config.startingRoot), l2BlockNumber: config.startingL2BlockNumber});

        // Deploy anchor state registry
        AnchorStateRegistry registry = deployAnchorStateRegistry(factory, portalAddress, startingAnchorRoot);

        // Deploy and configure access manager
        AccessManager accessManager = deployAccessManager(config, address(factoryProxy));

        // Deploy SP1 verifier and get configuration
        SP1Config memory sp1Config = deploySP1Verifier(config);

        // Deploy game implementation
        OPSuccinctFaultDisputeGame gameImpl =
            deployGameImplementation(config, factory, sp1Config, registry, accessManager);

        // Set initial bond and implementation in factory.
        factory.setInitBond(gameType, config.initialBondWei);
        factory.setImplementation(gameType, IDisputeGame(address(gameImpl)));

        // Create deployed contracts struct
        DeployedContracts memory deployedContracts = DeployedContracts({
            factoryProxy: address(factoryProxy),
            gameImplementation: address(gameImpl),
            sp1Verifier: sp1Config.verifierAddress,
            anchorStateRegistry: address(registry),
            accessManager: address(accessManager),
            optimismPortal2: portalAddress
        });

        return deployedContracts;
    }

    function deployGameImplementation(
        FDGConfig memory config,
        DisputeGameFactory factory,
        SP1Config memory sp1Config,
        AnchorStateRegistry registry,
        AccessManager accessManager
    ) internal returns (OPSuccinctFaultDisputeGame) {
        return new OPSuccinctFaultDisputeGame(
            Duration.wrap(uint64(config.maxChallengeDuration)),
            Duration.wrap(uint64(config.maxProveDuration)),
            IDisputeGameFactory(address(factory)),
            ISP1Verifier(sp1Config.verifierAddress),
            sp1Config.rollupConfigHash,
            sp1Config.aggregationVkey,
            sp1Config.rangeVkeyCommitment,
            config.challengerBondWei,
            IAnchorStateRegistry(address(registry)),
            accessManager
        );
    }

    function deployAnchorStateRegistry(
        DisputeGameFactory factory,
        address payable portalAddress,
        OutputRoot memory startingAnchorRoot
    ) internal returns (AnchorStateRegistry) {
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
        return registry;
    }

    function deployOrGetOptimismPortal2(FDGConfig memory config, GameType gameType) internal returns (address payable) {
        address payable portalAddress;
        if (config.optimismPortal2Address != address(0)) {
            portalAddress = payable(config.optimismPortal2Address);
            console.log("Using existing OptimismPortal2:", portalAddress);
        } else {
            MockOptimismPortal2 portal = new MockOptimismPortal2(gameType, config.disputeGameFinalityDelaySeconds);
            portalAddress = payable(address(portal));
            console.log("Deployed MockOptimismPortal2:", portalAddress);
        }
        return portalAddress;
    }

    function deploySP1Verifier(FDGConfig memory config) internal returns (SP1Config memory) {
        SP1Config memory sp1Config;
        sp1Config.rollupConfigHash = config.rollupConfigHash;
        sp1Config.aggregationVkey = config.aggregationVkey;
        sp1Config.rangeVkeyCommitment = config.rangeVkeyCommitment;

        // Get or deploy SP1 verifier based on configuration.
        if (config.useSp1MockVerifier) {
            // Deploy mock verifier for testing.
            SP1MockVerifier sp1Verifier = new SP1MockVerifier();
            sp1Config.verifierAddress = address(sp1Verifier);
            console.log("Using SP1 Mock Verifier:", address(sp1Verifier));
        } else {
            // Use provided verifier address for production.
            sp1Config.verifierAddress = config.verifierAddress;
            console.log("Using SP1 Verifier Gateway:", sp1Config.verifierAddress);
        }

        return sp1Config;
    }

    function deployAccessManager(FDGConfig memory config, address factoryAddress) internal returns (AccessManager) {
        // Deploy the access manager contract.
        AccessManager accessManager =
            new AccessManager(config.fallbackTimeoutFpSecs, IDisputeGameFactory(factoryAddress));
        console.log("Access manager:", address(accessManager));
        console.log("Permissionless fallback timeout (seconds):", config.fallbackTimeoutFpSecs);

        // Configure access control based on config.
        if (config.permissionlessMode) {
            // Set to permissionless games (anyone can propose and challenge).
            accessManager.setProposer(address(0), true);
            accessManager.setChallenger(address(0), true);
            console.log("Access Manager configured for permissionless mode");
        } else {
            // Set proposers.
            for (uint256 i = 0; i < config.proposerAddresses.length; i++) {
                if (config.proposerAddresses[i] != address(0)) {
                    accessManager.setProposer(config.proposerAddresses[i], true);
                    console.log("Added proposer:", config.proposerAddresses[i]);
                }
            }

            // Set challengers.
            for (uint256 i = 0; i < config.challengerAddresses.length; i++) {
                if (config.challengerAddresses[i] != address(0)) {
                    accessManager.setChallenger(config.challengerAddresses[i], true);
                    console.log("Added challenger:", config.challengerAddresses[i]);
                }
            }
        }

        return accessManager;
    }
}
