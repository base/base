// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test} from "forge-std/Test.sol";
import {AccessManager} from "../../src/fp/AccessManager.sol";

contract AccessManagerTest is Test {
    AccessManager accessManager;

    address owner = address(0x1234);
    address proposer1 = address(0x5678);
    address proposer2 = address(0x9ABC);
    address challenger1 = address(0xDEF0);
    address randomUser = address(0x1111);

    uint256 constant FALLBACK_TIMEOUT = 2 weeks; // 1209600 seconds
    uint256 constant SHORT_TIMEOUT = 1 hours; // For faster testing

    function setUp() public {
        vm.prank(owner);
        accessManager = new AccessManager(FALLBACK_TIMEOUT);
    }

    function testConstructor() public view {
        assertEq(accessManager.FALLBACK_TIMEOUT(), FALLBACK_TIMEOUT);
        assertEq(accessManager.lastProposalTimestamp(), block.timestamp);
        assertEq(accessManager.owner(), owner);
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

    function testIsAllowedProposer_TimeoutElapsed() public {
        // Initially, random user should not be allowed
        assertFalse(accessManager.isAllowedProposer(randomUser));

        // Warp time to exceed the fallback timeout
        vm.warp(block.timestamp + FALLBACK_TIMEOUT + 1);

        // Now anyone should be allowed to propose
        assertTrue(accessManager.isAllowedProposer(randomUser));
        assertTrue(accessManager.isAllowedProposer(proposer1));
        assertTrue(accessManager.isAllowedProposer(address(0x9999)));
    }

    function testIsAllowedProposer_TimeoutNotElapsed() public {
        // Warp to just before the timeout
        vm.warp(block.timestamp + FALLBACK_TIMEOUT - 1);

        // Random user should still not be allowed
        assertFalse(accessManager.isAllowedProposer(randomUser));
    }

    function testRecordProposal() public {
        uint256 initialTimestamp = accessManager.lastProposalTimestamp();

        // Warp time forward
        vm.warp(block.timestamp + 1000);

        // Record a proposal
        accessManager.recordProposal();

        // Timestamp should be updated
        assertEq(accessManager.lastProposalTimestamp(), block.timestamp);
        assertTrue(accessManager.lastProposalTimestamp() > initialTimestamp);
    }

    function testRecordProposal_ResetsTimeout() public {
        // Warp to near timeout
        vm.warp(block.timestamp + FALLBACK_TIMEOUT - 100);

        // Random user should not be allowed yet
        assertFalse(accessManager.isAllowedProposer(randomUser));

        // Record a proposal (reset the timer)
        accessManager.recordProposal();

        // Even after the original timeout would have elapsed, user should not be allowed
        vm.warp(block.timestamp + 200);
        assertFalse(accessManager.isAllowedProposer(randomUser));

        // But after the full timeout from the recorded proposal, they should be allowed
        vm.warp(block.timestamp + FALLBACK_TIMEOUT);
        assertTrue(accessManager.isAllowedProposer(randomUser));
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

    function testIsProposalPermissionlessMode() public {
        // Initially should be false
        assertFalse(accessManager.isProposalPermissionlessMode());

        // Warp to just before timeout
        vm.warp(block.timestamp + FALLBACK_TIMEOUT - 1);
        assertFalse(accessManager.isProposalPermissionlessMode());

        // Warp to exactly at timeout
        vm.warp(block.timestamp + 1);
        assertFalse(accessManager.isProposalPermissionlessMode());

        // Warp to after timeout
        vm.warp(block.timestamp + 1);
        assertTrue(accessManager.isProposalPermissionlessMode());
    }

    function testComplexScenario() public {
        // Set up approved proposer
        vm.prank(owner);
        accessManager.setProposer(proposer1, true);

        // Approved proposer can always propose
        assertTrue(accessManager.isAllowedProposer(proposer1));
        assertFalse(accessManager.isAllowedProposer(randomUser));

        // Warp to timeout - anyone can propose
        vm.warp(block.timestamp + FALLBACK_TIMEOUT + 1);
        assertTrue(accessManager.isAllowedProposer(proposer1));
        assertTrue(accessManager.isAllowedProposer(randomUser));

        // Record a proposal - resets the timer
        accessManager.recordProposal();

        // Now only approved proposers can propose again
        assertTrue(accessManager.isAllowedProposer(proposer1));
        assertFalse(accessManager.isAllowedProposer(randomUser));

        // Remove the approved proposer
        vm.prank(owner);
        accessManager.setProposer(proposer1, false);
        assertFalse(accessManager.isAllowedProposer(proposer1));
        assertFalse(accessManager.isAllowedProposer(randomUser));

        // Wait for timeout again
        vm.warp(block.timestamp + FALLBACK_TIMEOUT + 1);
        assertTrue(accessManager.isAllowedProposer(proposer1));
        assertTrue(accessManager.isAllowedProposer(randomUser));
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

contract AccessManagerShortTimeoutTest is Test {
    AccessManager accessManager;
    address owner = address(0x1234);
    address randomUser = address(0x1111);
    uint256 constant SHORT_TIMEOUT = 1 hours;

    function setUp() public {
        vm.prank(owner);
        accessManager = new AccessManager(SHORT_TIMEOUT);
    }

    function testShortTimeout() public {
        // Initially not allowed
        assertFalse(accessManager.isAllowedProposer(randomUser));

        // Warp past short timeout
        vm.warp(block.timestamp + SHORT_TIMEOUT + 1);

        // Now should be allowed
        assertTrue(accessManager.isAllowedProposer(randomUser));
    }
}
