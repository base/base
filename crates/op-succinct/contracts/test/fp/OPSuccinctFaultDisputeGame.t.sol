// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

// Testing
import "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

// Libraries
import {Claim, Duration, GameStatus, GameType, Hash, Timestamp} from "src/dispute/lib/Types.sol";
import {
    ClockNotExpired,
    IncorrectBondAmount,
    AlreadyInitialized,
    UnexpectedRootClaim,
    NoCreditToClaim
} from "src/dispute/lib/Errors.sol";
import {ParentGameNotResolved, InvalidParentGame, ClaimAlreadyChallenged, AlreadyProven} from "src/fp/lib/Errors.sol";
import {AggregationOutputs} from "src/lib/Types.sol";

// Contracts
import {DisputeGameFactory} from "src/dispute/DisputeGameFactory.sol";
import {OPSuccinctFaultDisputeGame} from "src/fp/OPSuccinctFaultDisputeGame.sol";
import {SP1MockVerifier} from "@sp1-contracts/src/SP1MockVerifier.sol";

// Interfaces
import {IDisputeGame} from "src/dispute/interfaces/IDisputeGame.sol";
import {IDisputeGameFactory} from "src/dispute/interfaces/IDisputeGameFactory.sol";
import {ISP1Verifier} from "@sp1-contracts/src/ISP1Verifier.sol";

contract OPSuccinctFaultDisputeGameTest is Test {
    // Event definitions matching those in OPSuccinctFaultDisputeGame
    event Challenged(address indexed challenger);
    event Proved(address indexed prover);
    event Resolved(GameStatus indexed status);

    DisputeGameFactory factory;
    ERC1967Proxy factoryProxy;

    OPSuccinctFaultDisputeGame gameImpl;
    OPSuccinctFaultDisputeGame parentGame;
    OPSuccinctFaultDisputeGame game;

    address proposer = address(0x123);
    address challenger = address(0x456);
    address prover = address(0x789);

    // Fixed parameters
    GameType gameType = GameType.wrap(42);
    Duration maxChallengeDuration = Duration.wrap(12 hours);
    Duration maxProveDuration = Duration.wrap(3 days);
    Claim rootClaim = Claim.wrap(keccak256("rootClaim"));

    // We will use these for the child game creation
    uint256 l2BlockNumber = 2000;
    uint32 parentIndex = 0;

    // For a new parent game that we manipulate separately in some tests
    OPSuccinctFaultDisputeGame separateParentGame;

    function setUp() public {
        // Deploy the implementation contract for DisputeGameFactory
        DisputeGameFactory factoryImpl = new DisputeGameFactory();

        // Deploy a proxy pointing to the factory implementation
        factoryProxy = new ERC1967Proxy(
            address(factoryImpl), abi.encodeWithSelector(DisputeGameFactory.initialize.selector, address(this))
        );

        // Cast the proxy to the factory contract
        factory = DisputeGameFactory(address(factoryProxy));

        // Create a mock verifier
        SP1MockVerifier sp1Verifier = new SP1MockVerifier();

        // Parameters for the OPSuccinctFaultDisputeGame
        bytes32 rollupConfigHash = bytes32(0);
        bytes32 aggregationVkey = bytes32(0);
        bytes32 rangeVkeyCommitment = bytes32(0);
        uint256 genesisL2BlockNumber = 0;
        bytes32 genesisL2OutputRoot = keccak256("genesis");
        uint256 proofReward = 1 ether;

        // Deploy the reference implementation of OPSuccinctFaultDisputeGame
        gameImpl = new OPSuccinctFaultDisputeGame(
            maxChallengeDuration,
            maxProveDuration,
            IDisputeGameFactory(address(factory)),
            ISP1Verifier(address(sp1Verifier)),
            rollupConfigHash,
            aggregationVkey,
            rangeVkeyCommitment,
            genesisL2BlockNumber,
            genesisL2OutputRoot,
            proofReward
        );

        // Set the init bond on the factory for our specific GameType
        factory.setInitBond(gameType, 1 ether);

        // Register our reference implementation under the specified gameType
        factory.setImplementation(gameType, IDisputeGame(address(gameImpl)));

        // Create the first (parent) game – it uses uint32.max as parent index
        vm.startPrank(proposer);
        vm.deal(proposer, 2 ether); // extra funds for testing

        // This parent game will be at index 0
        parentGame = OPSuccinctFaultDisputeGame(
            address(
                factory.create{value: 1 ether}(
                    gameType,
                    Claim.wrap(keccak256("genesis")),
                    // encode l2BlockNumber = 1000, parentIndex = uint32.max
                    abi.encodePacked(uint256(1000), type(uint32).max)
                )
            )
        );

        // We want the parent game to finalize. We'll skip its challenge period.
        (,,,,, Timestamp parentGameDeadline) = parentGame.claimData();
        vm.warp(parentGameDeadline.raw() + 1 seconds);
        parentGame.resolve();
        parentGame.claimCredit(proposer);

        // Create the child game referencing parent index = 0
        // The child game is at index 1
        game = OPSuccinctFaultDisputeGame(
            address(
                factory.create{value: 1 ether}(
                    gameType,
                    rootClaim,
                    // encode l2BlockNumber = 2000, parentIndex = 0
                    abi.encodePacked(l2BlockNumber, parentIndex)
                )
            )
        );

        vm.stopPrank();
    }

    // =========================================
    // Test: Basic initialization checks
    // =========================================
    function testInitialization() public view {
        // Test that the factory is correctly initialized
        assertEq(address(factory.owner()), address(this));
        assertEq(address(factory.gameImpls(gameType)), address(gameImpl));
        // We expect two games so far (parentGame at index 0, game at index 1)
        assertEq(factory.gameCount(), 2);

        // Check that the second game (our child game) matches the 'gameAtIndex(1)'
        (,, IDisputeGame proxy_) = factory.gameAtIndex(1);
        assertEq(address(game), address(proxy_));

        // Check the child game fields
        assertEq(game.gameType().raw(), gameType.raw());
        assertEq(game.rootClaim().raw(), rootClaim.raw());
        assertEq(game.maxChallengeDuration().raw(), maxChallengeDuration.raw());
        assertEq(game.maxProveDuration().raw(), maxProveDuration.raw());
        assertEq(address(game.disputeGameFactory()), address(factory));
        assertEq(game.l2BlockNumber(), l2BlockNumber);
        // The parent's block number was 1000
        assertEq(game.startingBlockNumber(), 1000);
        // The parent's root was keccak256("genesis")
        assertEq(game.startingRootHash().raw(), keccak256("genesis"));
        assertEq(address(game).balance, 1 ether);

        // Check the claimData
        (
            uint32 parentIndex_,
            address counteredBy_,
            address prover_,
            Claim claim_,
            OPSuccinctFaultDisputeGame.ProposalStatus status_,
            Timestamp deadline_
        ) = game.claimData();

        assertEq(parentIndex_, 0);
        assertEq(counteredBy_, address(0));
        assertEq(game.gameCreator(), proposer);
        assertEq(prover_, address(0));
        assertEq(claim_.raw(), rootClaim.raw());
        // Initially, the status is Unchallenged
        assertEq(uint8(status_), uint8(OPSuccinctFaultDisputeGame.ProposalStatus.Unchallenged));
        // The child's initial deadline is block.timestamp + maxChallengeDuration
        uint256 currentTime = block.timestamp;
        uint256 expectedDeadline = currentTime + maxChallengeDuration.raw();
        assertEq(deadline_.raw(), expectedDeadline);
    }

    // =========================================
    // Test: Resolve unchallenged
    // =========================================
    function testResolveUnchallenged() public {
        assertEq(uint8(game.status()), uint8(GameStatus.IN_PROGRESS));

        // Should revert if we try to resolve before deadline
        vm.expectRevert(ClockNotExpired.selector);
        game.resolve();

        // Warp forward past the challenge deadline
        (,,,,, Timestamp deadline) = game.claimData();
        vm.warp(deadline.raw() + 1);

        // Expect the Resolved event
        vm.expectEmit(true, false, false, false, address(game));
        emit Resolved(GameStatus.DEFENDER_WINS);

        // Now we can resolve successfully
        game.resolve();

        // Proposer gets the bond back
        game.claimCredit(proposer);

        // Check final state
        assertEq(uint8(game.status()), uint8(GameStatus.DEFENDER_WINS));
        // The contract should have paid back the proposer
        assertEq(address(game).balance, 0);
        // Proposer posted 1 ether, so they get it back
        assertEq(proposer.balance, 2 ether);
        assertEq(challenger.balance, 0);
    }

    // =========================================
    // Test: Resolve unchallenged + prove
    // =========================================
    function testResolveUnchallengedAndValidProofProvided() public {
        assertEq(uint8(game.status()), uint8(GameStatus.IN_PROGRESS));

        // Should revert if we try to resolve before the first challenge deadline
        vm.expectRevert(ClockNotExpired.selector);
        game.resolve();

        // Prover proves the claim while unchallenged
        vm.startPrank(prover);
        game.prove(bytes(""));
        vm.stopPrank();

        // Now the proposal is UnchallengedAndValidProofProvided; we can resolve immediately
        game.resolve();

        // Prover does not get any credit
        vm.expectRevert(NoCreditToClaim.selector);
        game.claimCredit(prover);

        // Proposer gets the bond back
        game.claimCredit(proposer);

        // Final status: DEFENDER_WINS
        assertEq(uint8(game.status()), uint8(GameStatus.DEFENDER_WINS));
        assertEq(address(game).balance, 0);

        // Proposer gets their 1 ether back
        assertEq(proposer.balance, 2 ether);
        // Prover does NOT get the reward because no challenger posted a bond
        assertEq(prover.balance, 0 ether);
        assertEq(challenger.balance, 0);
    }

    // =========================================
    // Test: Resolve challenged + valid proof
    // =========================================
    function testResolveChallengedAndValidProofProvided() public {
        assertEq(uint8(game.status()), uint8(GameStatus.IN_PROGRESS));
        assertEq(address(game).balance, 1 ether);

        // Try to resolve too early
        vm.expectRevert(ClockNotExpired.selector);
        game.resolve();

        // Challenger posts the bond incorrectly
        vm.startPrank(challenger);
        vm.deal(challenger, 1 ether);

        // Must pay exactly the required bond
        vm.expectRevert(IncorrectBondAmount.selector);
        game.challenge{value: 0.5 ether}();

        // Correctly challenge
        game.challenge{value: 1 ether}();
        vm.stopPrank();

        // Now the contract holds 2 ether total
        assertEq(address(game).balance, 2 ether);

        // Confirm the proposal is in Challenged state
        (, address counteredBy_,,, OPSuccinctFaultDisputeGame.ProposalStatus challStatus,) = game.claimData();
        assertEq(counteredBy_, challenger);
        assertEq(uint8(challStatus), uint8(OPSuccinctFaultDisputeGame.ProposalStatus.Challenged));

        // Prover proves the claim in time
        vm.startPrank(prover);
        game.prove(bytes(""));
        vm.stopPrank();

        // Confirm the proposal is now ChallengedAndValidProofProvided
        (,,,, challStatus,) = game.claimData();
        assertEq(uint8(challStatus), uint8(OPSuccinctFaultDisputeGame.ProposalStatus.ChallengedAndValidProofProvided));
        assertEq(uint8(game.status()), uint8(GameStatus.IN_PROGRESS));

        // Resolve
        game.resolve();

        // Prover gets the proof reward
        game.claimCredit(prover);

        // Proposer gets the bond back
        game.claimCredit(proposer);

        assertEq(uint8(game.status()), uint8(GameStatus.DEFENDER_WINS));
        assertEq(address(game).balance, 0);

        // Final balances:
        // - The proposer recovers their 1 ether stake
        // - The prover gets 1 ether reward
        // - The challenger gets nothing
        assertEq(proposer.balance, 2 ether);
        assertEq(prover.balance, 1 ether);
        assertEq(challenger.balance, 0);
    }

    // =========================================
    // Test: Resolve challenged but not proven
    // =========================================
    function testResolveChallengedAndNoProof() public {
        // Challenge the game
        vm.startPrank(challenger);
        vm.deal(challenger, 2 ether);
        game.challenge{value: 1 ether}();
        vm.stopPrank();

        // The contract now has 2 ether total
        assertEq(address(game).balance, 2 ether);

        // We must wait for the prove deadline to pass
        (,,,,, Timestamp deadline) = game.claimData();
        vm.warp(deadline.raw() + 1);

        // Now we can resolve, resulting in CHALLENGER_WINS
        game.resolve();

        // Challenger gets the bond back and wins proposer's bond
        game.claimCredit(challenger);

        assertEq(uint8(game.status()), uint8(GameStatus.CHALLENGER_WINS));

        // The challenger receives the entire 3 ether
        assertEq(challenger.balance, 3 ether); // started with 2, spent 1, got 2 from the game

        // The proposer loses their 1 ether stake
        assertEq(proposer.balance, 1 ether); // started with 2, lost 1
        // The contract balance is zero
        assertEq(address(game).balance, 0);
    }

    // =========================================
    // Test: Attempting multiple challenges
    // =========================================
    function testCannotChallengeMultipleTimes() public {
        // Initially unchallenged
        (, address counteredBy_,,, OPSuccinctFaultDisputeGame.ProposalStatus status_,) = game.claimData();
        assertEq(counteredBy_, address(0));
        assertEq(uint8(status_), uint8(OPSuccinctFaultDisputeGame.ProposalStatus.Unchallenged));

        // The first challenge is valid
        vm.startPrank(challenger);
        vm.deal(challenger, 2 ether);
        game.challenge{value: 1 ether}();

        // A second challenge from any party should revert because the proposal is no longer "Unchallenged"
        vm.expectRevert(ClaimAlreadyChallenged.selector);
        game.challenge{value: 1 ether}();
        vm.stopPrank();
    }

    // =========================================
    // Test: Attempt to prove after the prove deadline
    // =========================================
    function testCannotProveAfterDeadline() public {
        // Challenge first
        vm.startPrank(challenger);
        vm.deal(challenger, 1 ether);
        game.challenge{value: 1 ether}();
        vm.stopPrank();

        // Move time forward beyond the prove period
        (,,,,, Timestamp deadline) = game.claimData();
        vm.warp(deadline.raw() + 1);

        vm.startPrank(prover);
        // Attempting to prove after the deadline is exceeded
        vm.expectRevert();
        game.prove(bytes(""));
        vm.stopPrank();
    }

    // =========================================
    // Test: Attempt to create a game with rootBlock <= parentBlock
    // This triggers UnexpectedRootClaim in initialize().
    // =========================================
    function testCannotCreateChildWithSmallerBlockThanParent() public {
        // The parent game used L2 block 1234567890
        // Try to create a child game that references l2BlockNumber = 1
        vm.startPrank(proposer);
        vm.deal(proposer, 1 ether);

        // We expect revert
        vm.expectRevert(
            abi.encodeWithSelector(
                UnexpectedRootClaim.selector,
                Claim.wrap(keccak256("rootClaim")) // The rootClaim we pass
            )
        );

        factory.create{value: 1 ether}(
            gameType,
            rootClaim,
            abi.encodePacked(uint256(1), uint32(0)) // L2 block is smaller than parent's block
        );
        vm.stopPrank();
    }

    // =========================================
    // Test: Parent game is still in progress -> child game cannot resolve
    // =========================================
    function testCannotResolveIfParentGameInProgress() public {
        vm.startPrank(proposer);

        // Create a new game with parentIndex = 1
        OPSuccinctFaultDisputeGame childGame = OPSuccinctFaultDisputeGame(
            address(
                factory.create{value: 1 ether}(
                    gameType,
                    Claim.wrap(keccak256("new-claim")),
                    // encode l2BlockNumber = 3000, parentIndex = 1
                    abi.encodePacked(uint256(3000), uint32(1))
                )
            )
        );

        vm.stopPrank();

        // The parent game is still in progress, not resolved
        // So, if we try to resolve the childGame, it should revert with ParentGameNotResolved
        vm.expectRevert(ParentGameNotResolved.selector);
        childGame.resolve();
    }

    // =========================================
    // Test: Parent game is invalid -> child game is immediately resolved as CHALLENGER_WINS
    // Because the parent's claim is invalid, the child should be auto-lost.
    // =========================================
    function testParentGameChallengerWinsInvalidatesChild() public {
        // 1) Now create a child game referencing that losing parent at index 1
        vm.startPrank(proposer);
        OPSuccinctFaultDisputeGame childGame = OPSuccinctFaultDisputeGame(
            address(
                factory.create{value: 1 ether}(
                    gameType, Claim.wrap(keccak256("child-of-loser")), abi.encodePacked(uint256(10000), uint32(1))
                )
            )
        );
        vm.stopPrank();

        // 2) Challenge the parent game so that it ends up CHALLENGER_WINS when proof is not provided within the prove deadline
        vm.startPrank(challenger);
        vm.deal(challenger, 2 ether);
        game.challenge{value: 1 ether}();
        vm.stopPrank();

        // 3) Warp past the prove deadline
        (,,,,, Timestamp gameDeadline) = game.claimData();
        vm.warp(gameDeadline.raw() + 1);

        // 4) The game resolves as CHALLENGER_WINS
        game.resolve();

        // Challenger gets the bond back and wins proposer's bond
        game.claimCredit(challenger);

        assertEq(uint8(game.status()), uint8(GameStatus.CHALLENGER_WINS));

        // 5) If we try to resolve the child game, it should be resolved as CHALLENGER_WINS
        // because parent's claim is invalid.
        // The child's bond is lost since there is no challenger for the child game.
        childGame.resolve();

        // Challenger hasn't challenged the child game, so it gets nothing
        vm.expectRevert(NoCreditToClaim.selector);
        childGame.claimCredit(challenger);

        assertEq(uint8(childGame.status()), uint8(GameStatus.CHALLENGER_WINS));

        assertEq(address(childGame).balance, 1 ether);
        assertEq(address(challenger).balance, 3 ether);
        assertEq(address(proposer).balance, 0 ether);
    }

    // =========================================
    // Test: Attempting multiple `prove()` calls
    // =========================================
    function testCannotProveMultipleTimes() public {
        vm.startPrank(prover);
        game.prove(bytes(""));
        vm.expectRevert(AlreadyProven.selector);
        game.prove(bytes(""));
        vm.stopPrank();
    }
}
