// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

/// @title AccessManager
/// @notice Manages permissions for dispute game proposers and challengers.
contract AccessManager is Ownable {
    ////////////////////////////////////////////////////////////////
    //                         Events                             //
    ////////////////////////////////////////////////////////////////

    /// @notice Event emitted when proposer permissions are updated.
    event ProposerPermissionUpdated(address indexed proposer, bool allowed);

    /// @notice Event emitted when challenger permissions are updated.
    event ChallengerPermissionUpdated(address indexed challenger, bool allowed);

    ////////////////////////////////////////////////////////////////
    //                         State Vars                         //
    ////////////////////////////////////////////////////////////////

    /// @notice Tracks whitelisted proposers.
    mapping(address => bool) public proposers;

    /// @notice Tracks whitelisted challengers.
    mapping(address => bool) public challengers;

    /// @notice The timeout (in seconds) after which permissionless proposing is allowed (immutable).
    uint256 public immutable FALLBACK_TIMEOUT;

    /// @notice The timestamp of the last proposal action.
    uint256 public lastProposalTimestamp;

    ////////////////////////////////////////////////////////////////
    //                      Constructor                           //
    ////////////////////////////////////////////////////////////////

    /// @notice Constructor sets the fallback timeout and initializes timestamp.
    /// @param _fallbackTimeout The timeout in seconds after last proposal when permissionless mode activates.
    constructor(uint256 _fallbackTimeout) {
        FALLBACK_TIMEOUT = _fallbackTimeout;
        // Initialize timestamp to deployment time
        lastProposalTimestamp = block.timestamp;
    }

    ////////////////////////////////////////////////////////////////
    //                      Functions                             //
    ////////////////////////////////////////////////////////////////

    /**
     * @notice Allows the owner to whitelist or un-whitelist proposers.
     * @param _proposer The address to set in the proposers mapping.
     * @param _allowed True if whitelisting, false otherwise.
     */
    function setProposer(address _proposer, bool _allowed) external onlyOwner {
        proposers[_proposer] = _allowed;
        emit ProposerPermissionUpdated(_proposer, _allowed);
    }

    /**
     * @notice Allows the owner to whitelist or un-whitelist challengers.
     * @param _challenger The address to set in the challengers mapping.
     * @param _allowed True if whitelisting, false otherwise.
     */
    function setChallenger(address _challenger, bool _allowed) external onlyOwner {
        challengers[_challenger] = _allowed;
        emit ChallengerPermissionUpdated(_challenger, _allowed);
    }

    /// @notice Checks if an address is allowed to propose.
    /// @param _proposer The address to check.
    /// @return allowed_ Whether the address is allowed to propose.
    function isAllowedProposer(address _proposer) external view returns (bool allowed_) {
        // If address(0) is allowed, then it's permissionless.
        // If the fallback timeout has elapsed since last proposal, anyone can propose.
        allowed_ = proposers[address(0)] || proposers[_proposer]
            || (block.timestamp - lastProposalTimestamp > FALLBACK_TIMEOUT);
    }

    /// @notice Checks if an address is allowed to challenge.
    /// @param _challenger The address to check.
    /// @return allowed_ Whether the address is allowed to challenge.
    function isAllowedChallenger(address _challenger) external view returns (bool allowed_) {
        // If address(0) is allowed, then it's permissionless.
        allowed_ = challengers[address(0)] || challengers[_challenger];
    }

    /// @notice Updates the last proposal timestamp. Should be called when a proposal is made.
    /// @dev This function should be called by the game contract when a proposal is made.
    function recordProposal() external {
        lastProposalTimestamp = block.timestamp;
    }

    /// @notice Returns whether proposal fallback timeout has elapsed.
    /// @return Whether permissionless proposing is active.
    function isProposalPermissionlessMode() external view returns (bool) {
        return block.timestamp - lastProposalTimestamp > FALLBACK_TIMEOUT || proposers[address(0)];
    }
}
