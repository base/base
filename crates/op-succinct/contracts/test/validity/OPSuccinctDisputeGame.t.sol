// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

// Testing
import "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

// Libraries
import {Claim, GameStatus, GameType, GameTypes, Hash, OutputRoot, Timestamp} from "src/dispute/lib/Types.sol";
import {AlreadyInitialized, GameNotInProgress, NoCreditToClaim, GameNotFinalized} from "src/dispute/lib/Errors.sol";
import {Utils} from "../helpers/Utils.sol";

// Contracts
import {DisputeGameFactory} from "src/dispute/DisputeGameFactory.sol";
import {OPSuccinctDisputeGame} from "src/validity/OPSuccinctDisputeGame.sol";
import {OPSuccinctL2OutputOracle} from "src/validity/OPSuccinctL2OutputOracle.sol";
import {AnchorStateRegistry} from "src/dispute/AnchorStateRegistry.sol";
import {SuperchainConfig} from "src/L1/SuperchainConfig.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";

// Interfaces
import {IDisputeGame} from "interfaces/dispute/IDisputeGame.sol";
import {IDisputeGameFactory} from "interfaces/dispute/IDisputeGameFactory.sol";
import {ISuperchainConfig} from "interfaces/L1/ISuperchainConfig.sol";
import {IOptimismPortal2} from "interfaces/L1/IOptimismPortal2.sol";
import {IAnchorStateRegistry} from "interfaces/dispute/IAnchorStateRegistry.sol";

// Utils
import {MockOptimismPortal2} from "../../utils/MockOptimismPortal2.sol";

contract OPSuccinctDisputeGameTest is Test, Utils {
    // Event definitions matching those in OPSuccinctDisputeGame.
    event Resolved(GameStatus indexed status);

    DisputeGameFactory factory;
    ERC1967Proxy factoryProxy;

    OPSuccinctDisputeGame gameImpl;
    OPSuccinctDisputeGame game;

    OPSuccinctL2OutputOracle l2OutputOracle;

    address proposer = address(0x123);

    // Fixed parameters.
    GameType gameType = GameTypes.OP_SUCCINCT;
    Claim rootClaim = Claim.wrap(keccak256("rootClaim"));

    // Game creation parameters.
    uint256 l2BlockNumber = 2000;
    uint256 l1BlockNumber = 1000;

    function setUp() public {
        // Deploy the implementation contract for DisputeGameFactory.
        DisputeGameFactory factoryImpl = new DisputeGameFactory();

        // Deploy a proxy pointing to the factory implementation.
        factoryProxy = new ERC1967Proxy(
            address(factoryImpl), abi.encodeWithSelector(DisputeGameFactory.initialize.selector, address(this))
        );

        // Cast the proxy to the factory contract.
        factory = DisputeGameFactory(address(factoryProxy));

        // Deploy L2OutputOracle using Utils helper functions.
        (l2OutputOracle,) = deployL2OutputOracleWithStandardParams(proposer, address(0), address(this));

        // Deploy the implementation of OPSuccinctDisputeGame.
        gameImpl = new OPSuccinctDisputeGame(address(l2OutputOracle));

        // Register our reference implementation under the specified gameType.
        factory.setImplementation(gameType, IDisputeGame(address(gameImpl)));

        // Create a game
        vm.startBroadcast(proposer);

        // Warp time forward to ensure the game is created after the respectedGameTypeUpdatedAt timestamp.
        warpRollAndCheckpoint(l2OutputOracle, 4001, l1BlockNumber);

        bytes memory proof = bytes("");
        game = OPSuccinctDisputeGame(
            address(
                factory.create(
                    gameType,
                    rootClaim,
                    abi.encodePacked(
                        l2BlockNumber, l1BlockNumber, proposer, l2OutputOracle.GENESIS_CONFIG_NAME(), proof
                    )
                )
            )
        );

        vm.stopBroadcast();
    }

    // =========================================
    // Test: Basic initialization checks
    // =========================================
    function testInitialization() public view {
        // Test that the factory is correctly initialized.
        assertEq(address(factory.owner()), address(this));
        assertEq(address(factory.gameImpls(gameType)), address(gameImpl));
        assertEq(factory.gameCount(), 1);

        // Check that the game matches the 'gameAtIndex(0)'.
        (,, IDisputeGame proxy_) = factory.gameAtIndex(0);
        assertEq(address(game), address(proxy_));

        // Check the game fields.
        assertEq(game.gameType().raw(), gameType.raw());
        assertEq(game.gameCreator(), proposer);
        assertEq(game.rootClaim().raw(), rootClaim.raw());
        assertEq(game.l2BlockNumber(), l2BlockNumber);
        assertEq(game.l1BlockNumber(), l1BlockNumber);
        assertEq(game.proverAddress(), proposer);
        assertEq(game.configName(), l2OutputOracle.GENESIS_CONFIG_NAME());
        assertEq(keccak256(game.proof()), keccak256(bytes("")));
        assertEq(uint8(game.status()), uint8(GameStatus.DEFENDER_WINS));
    }

    // =========================================
    // Test: Cannot resolve game twice
    // =========================================
    function testCannotResolveTwice() public {
        vm.expectRevert(GameNotInProgress.selector);
        game.resolve();
    }

    // =========================================
    // Test: Cannot re-initialize game
    // =========================================
    function testCannotReInitializeGame() public {
        vm.startBroadcast(proposer);
        vm.expectRevert("L2OutputOracle: block number must be greater than or equal to next expected block number");
        game.initialize();
        vm.stopBroadcast();
    }

    // =========================================
    // Test: Cannot create game without permission
    // =========================================
    function testCannotCreateGameWithoutPermission() public {
        address maliciousProposer = address(0x1234);

        vm.startPrank(maliciousProposer);
        vm.deal(maliciousProposer, 1 ether);

        // Warp forward to the block we want to propose and checkpoint
        uint256 newL1BlockNumber = l1BlockNumber + 500;
        warpRollAndCheckpoint(l2OutputOracle, 2000, newL1BlockNumber);

        bytes memory proof = bytes("");
        bytes32 configName = l2OutputOracle.GENESIS_CONFIG_NAME();
        vm.expectRevert("L2OutputOracle: only approved proposers can propose new outputs");
        factory.create(
            gameType,
            Claim.wrap(keccak256("new-claim")),
            abi.encodePacked(l2BlockNumber + 1000, newL1BlockNumber, maliciousProposer, configName, proof)
        );

        vm.stopPrank();
    }
}
