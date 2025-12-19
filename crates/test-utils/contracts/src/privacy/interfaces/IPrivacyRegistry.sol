// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {SlotConfig} from "../types/PrivacyTypes.sol";

/// @title IPrivacyRegistry
/// @author Base Privacy Team
/// @notice Interface for the Privacy Registry precompile at 0x0200
/// @dev Allows contracts to register for private storage protection
///
/// @dev Example usage:
/// ```solidity
/// IPrivacyRegistry registry = IPrivacyRegistry(PRIVACY_REGISTRY);
/// SlotConfig[] memory slots = new SlotConfig[](1);
/// slots[0] = SlotConfig(0, SlotType.Mapping, OwnershipType.MappingKey);
/// registry.register(msg.sender, slots, false);
/// ```
///
/// @custom:address 0x0000000000000000000000000000000000000200
/// @custom:gas-base 5000
/// @custom:gas-per-slot 2000
interface IPrivacyRegistry {
    /*//////////////////////////////////////////////////////////////
                                 ERRORS
    //////////////////////////////////////////////////////////////*/

    /// @notice Thrown when attempting to register an already registered contract
    error AlreadyRegistered();

    /// @notice Thrown when the contract is not registered
    error NotRegistered();

    /// @notice Thrown when caller is not authorized (not admin or contract)
    error Unauthorized();

    /// @notice Thrown when slot configuration is invalid
    error InvalidSlotConfig();

    /// @notice Thrown when the precompile call fails
    error PrecompileCallFailed();

    /*//////////////////////////////////////////////////////////////
                                 EVENTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Emitted when a contract registers for privacy protection
    /// @param contract_ The contract that registered
    /// @param admin The admin address that can modify the registration
    /// @param slotCount Number of slots registered
    event ContractRegistered(
        address indexed contract_,
        address indexed admin,
        uint256 slotCount
    );

    /// @notice Emitted when additional slots are added to a registration
    /// @param contract_ The contract being updated
    /// @param newSlotCount Number of new slots added
    event SlotsAdded(address indexed contract_, uint256 newSlotCount);

    /// @notice Emitted when the hideEvents setting is changed
    /// @param contract_ The contract being updated
    /// @param hide The new hideEvents value
    event HideEventsUpdated(address indexed contract_, bool hide);

    /*//////////////////////////////////////////////////////////////
                           REGISTRATION FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Register a contract for private storage protection
    /// @dev Only callable by the contract itself (msg.sender will be the caller)
    /// @dev Gas cost: ~5000 base + 2000 per slot
    /// @param admin Address authorized to modify this registration later
    /// @param slots Array of slot configurations to protect
    /// @param hideEvents Whether to filter events from this contract for unauthorized viewers
    function register(
        address admin,
        SlotConfig[] calldata slots,
        bool hideEvents
    ) external;

    /// @notice Add additional private slots to an existing registration
    /// @dev Only callable by the registered admin or the contract itself
    /// @dev Gas cost: ~5000 base + 2000 per slot
    /// @param slots Additional slot configurations to protect
    function addSlots(SlotConfig[] calldata slots) external;

    /// @notice Update the hideEvents setting for a registered contract
    /// @dev Only callable by the registered admin or the contract itself
    /// @dev Gas cost: ~5000
    /// @param hide Whether to filter events from this contract
    function setHideEvents(bool hide) external;

    /*//////////////////////////////////////////////////////////////
                              VIEW FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if a contract is registered for privacy protection
    /// @dev Gas cost: ~100
    /// @param contract_ The contract address to check
    /// @return registered True if the contract is registered
    function isRegistered(address contract_) external view returns (bool registered);
}
