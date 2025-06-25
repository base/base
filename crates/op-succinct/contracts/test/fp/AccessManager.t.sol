// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test} from "forge-std/Test.sol";
import {AccessManager} from "../../src/fp/AccessManager.sol";
import {IDisputeGameFactory} from "interfaces/dispute/IDisputeGameFactory.sol";
import {GameType} from "src/dispute/lib/Types.sol";

// Mock factory that returns empty results for findLatestGames
contract MockDisputeGameFactory {
    function findLatestGames(GameType, uint256, uint256)
        external
        pure
        returns (IDisputeGameFactory.GameSearchResult[] memory)
    {
        // Return empty array
        return new IDisputeGameFactory.GameSearchResult[](0);
    }

    function gameCount() external pure returns (uint256) {
        return 0;
    }
}

contract AccessManagerTest is Test {
    AccessManager accessManager;

    address owner = address(0x1234);
    address proposer1 = address(0x5678);
    address proposer2 = address(0x9ABC);
    address challenger1 = address(0xDEF0);
    address randomUser = address(0x1111);
    IDisputeGameFactory mockFactory;

    uint256 constant FALLBACK_TIMEOUT = 2 weeks; // 1209600 seconds
    uint256 constant SHORT_TIMEOUT = 1 hours; // For faster testing

    function setUp() public {
        // Deploy mock factory
        MockDisputeGameFactory mockFactoryImpl = new MockDisputeGameFactory();
        mockFactory = IDisputeGameFactory(address(mockFactoryImpl));

        vm.prank(owner);
        accessManager = new AccessManager(FALLBACK_TIMEOUT, mockFactory);
    }

    function testConstructor() public view {
        assertEq(accessManager.FALLBACK_TIMEOUT(), FALLBACK_TIMEOUT);
        assertEq(accessManager.owner(), owner);
        assertEq(address(accessManager.DISPUTE_GAME_FACTORY()), address(mockFactory));
    }

    function testSetProposer() public {
        vm.prank(owner);
        accessManager.setProposer(proposer1, true);
        assertTrue(accessManager.proposers(proposer1));

        vm.prank(owner);
        accessManager.setProposer(proposer1, false);
        assertFalse(accessManager.proposers(proposer1));
    }

    function testSetProposerOnlyOwner() public {
        vm.prank(randomUser);
        vm.expectRevert("Ownable: caller is not the owner");
        accessManager.setProposer(proposer1, true);
    }

    function testSetChallenger() public {
        vm.prank(owner);
        accessManager.setChallenger(challenger1, true);
        assertTrue(accessManager.challengers(challenger1));

        vm.prank(owner);
        accessManager.setChallenger(challenger1, false);
        assertFalse(accessManager.challengers(challenger1));
    }

    function testSetChallengerOnlyOwner() public {
        vm.prank(randomUser);
        vm.expectRevert("Ownable: caller is not the owner");
        accessManager.setChallenger(challenger1, true);
    }

    function testIsAllowedProposer_ApprovedProposer() public {
        vm.prank(owner);
        accessManager.setProposer(proposer1, true);

        assertTrue(accessManager.isAllowedProposer(proposer1));
        assertFalse(accessManager.isAllowedProposer(randomUser));
    }

    function testIsAllowedProposer_PermissionlessMode() public {
        vm.prank(owner);
        accessManager.setProposer(address(0), true);

        assertTrue(accessManager.isAllowedProposer(proposer1));
        assertTrue(accessManager.isAllowedProposer(randomUser));
        assertTrue(accessManager.isAllowedProposer(address(0x9999)));
    }

    function testIsAllowedChallenger_ApprovedChallenger() public {
        vm.prank(owner);
        accessManager.setChallenger(challenger1, true);

        assertTrue(accessManager.isAllowedChallenger(challenger1));
        assertFalse(accessManager.isAllowedChallenger(randomUser));
    }

    function testIsAllowedChallenger_PermissionlessMode() public {
        vm.prank(owner);
        accessManager.setChallenger(address(0), true);

        assertTrue(accessManager.isAllowedChallenger(challenger1));
        assertTrue(accessManager.isAllowedChallenger(randomUser));
        assertTrue(accessManager.isAllowedChallenger(address(0x9999)));
    }

    function testIsAllowedChallenger_NoTimeoutLogic() public {
        // Challenger logic should NOT have timeout - warp time way forward
        vm.warp(block.timestamp + FALLBACK_TIMEOUT + 1000);

        // Random user should still not be allowed to challenge (no timeout for challengers)
        assertFalse(accessManager.isAllowedChallenger(randomUser));

        // Only if explicitly allowed
        vm.prank(owner);
        accessManager.setChallenger(randomUser, true);
        assertTrue(accessManager.isAllowedChallenger(randomUser));
    }

    function testEvents() public {
        vm.prank(owner);
        vm.expectEmit(true, false, false, true);
        emit ProposerPermissionUpdated(proposer1, true);
        accessManager.setProposer(proposer1, true);

        vm.prank(owner);
        vm.expectEmit(true, false, false, true);
        emit ChallengerPermissionUpdated(challenger1, true);
        accessManager.setChallenger(challenger1, true);
    }

    // Event declarations for testing
    event ProposerPermissionUpdated(address indexed proposer, bool allowed);
    event ChallengerPermissionUpdated(address indexed challenger, bool allowed);
}
