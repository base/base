// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {IPrivacyRegistry} from "../interfaces/IPrivacyRegistry.sol";
import {IPrivacyAuth} from "../interfaces/IPrivacyAuth.sol";
import {SlotConfig, SlotType, OwnershipType, Permission, PRIVACY_REGISTRY, PRIVACY_AUTH} from "../types/PrivacyTypes.sol";
import {PrivacyLib} from "../libraries/PrivacyLib.sol";

/// @title PrivacyEnabled
/// @author Base Privacy Team
/// @notice Abstract base contract for privacy-enabled contracts
/// @dev Inherit this contract to easily register private storage slots
///
/// @dev Usage pattern:
/// ```solidity
/// contract MyContract is PrivacyEnabled {
///     mapping(address => uint256) private _balances; // slot 0
///
///     constructor() {
///         _registerPrivateMapping(0);
///         _finalizePrivacyRegistration();
///     }
/// }
/// ```
///
/// @dev Important: Call registration functions in the constructor before any
/// storage is written to ensure slots are protected from the start.
abstract contract PrivacyEnabled {
    using PrivacyLib for SlotConfig;

    /*//////////////////////////////////////////////////////////////
                                 ERRORS
    //////////////////////////////////////////////////////////////*/

    /// @notice Thrown when privacy registration fails
    error PrivacyRegistrationFailed();

    /// @notice Thrown when already registered for privacy
    error AlreadyRegisteredForPrivacy();

    /// @notice Thrown when trying to add slots before registration is finalized
    error PrivacyNotFinalized();

    /// @notice Thrown when no slots are registered before finalization
    error NoSlotsRegistered();

    /// @notice Thrown when an invalid slot config is provided
    error InvalidSlotConfiguration();

    /*//////////////////////////////////////////////////////////////
                                 EVENTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Emitted when privacy registration is finalized
    /// @param admin The privacy admin address
    /// @param slotCount Number of slots registered
    event PrivacyRegistrationFinalized(address indexed admin, uint256 slotCount);

    /*//////////////////////////////////////////////////////////////
                                 STATE
    //////////////////////////////////////////////////////////////*/

    /// @dev Pending slot configurations before finalization
    SlotConfig[] private _pendingSlots;

    /// @dev Whether this contract has been registered for privacy
    bool private _privacyRegistered;

    /// @dev Privacy admin (can modify registration)
    address private _privacyAdmin;

    /*//////////////////////////////////////////////////////////////
                               MODIFIERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Ensures caller is the privacy admin
    modifier onlyPrivacyAdmin() {
        require(msg.sender == _privacyAdmin, "PrivacyEnabled: not admin");
        _;
    }

    /*//////////////////////////////////////////////////////////////
                     SLOT REGISTRATION (CONSTRUCTOR)
    //////////////////////////////////////////////////////////////*/

    /// @notice Register a simple private slot (contract owns it)
    /// @dev Call in constructor before _finalizePrivacyRegistration()
    /// @param slot The storage slot number
    function _registerPrivateSlot(uint256 slot) internal {
        _pendingSlots.push(PrivacyLib.simpleSlot(slot));
    }

    /// @notice Register a mapping where the key address owns their entry
    /// @dev Use for: mapping(address => X) where address should own their data
    /// @dev Call in constructor before _finalizePrivacyRegistration()
    /// @param slot The mapping's base storage slot
    function _registerPrivateMapping(uint256 slot) internal {
        _pendingSlots.push(PrivacyLib.mappingKeyOwned(slot));
    }

    /// @notice Register an allowances-style nested mapping (outer key owns)
    /// @dev Use for: mapping(owner => mapping(spender => X))
    /// @dev Call in constructor before _finalizePrivacyRegistration()
    /// @param slot The mapping's base storage slot
    function _registerPrivateAllowances(uint256 slot) internal {
        _pendingSlots.push(PrivacyLib.allowancesSlot(slot));
    }

    /// @notice Register a delegation-style nested mapping (inner key owns)
    /// @dev Use for: mapping(delegator => mapping(delegate => X))
    /// @dev Call in constructor before _finalizePrivacyRegistration()
    /// @param slot The mapping's base storage slot
    function _registerPrivateDelegation(uint256 slot) internal {
        _pendingSlots.push(PrivacyLib.delegationSlot(slot));
    }

    /// @notice Register a custom slot configuration
    /// @dev Call in constructor before _finalizePrivacyRegistration()
    /// @param config The slot configuration
    function _registerPrivateSlotConfig(SlotConfig memory config) internal {
        if (!config.isValid()) revert InvalidSlotConfiguration();
        _pendingSlots.push(config);
    }

    /*//////////////////////////////////////////////////////////////
                         REGISTRATION FINALIZATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Finalize privacy registration with the precompile
    /// @dev Must be called after all slots are registered in constructor
    /// @dev This calls the Registry precompile to register this contract
    /// @param admin Address that can modify registration (use address(this) for contract-only)
    /// @param hideEvents Whether to filter events from public view
    function _finalizePrivacyRegistration(address admin, bool hideEvents) internal {
        if (_privacyRegistered) revert AlreadyRegisteredForPrivacy();
        if (_pendingSlots.length == 0) revert NoSlotsRegistered();

        _privacyAdmin = admin;

        // Call the registry precompile
        (bool success, ) = PRIVACY_REGISTRY.call(
            abi.encodeWithSelector(
                IPrivacyRegistry.register.selector,
                admin,
                _pendingSlots,
                hideEvents
            )
        );

        if (!success) revert PrivacyRegistrationFailed();

        _privacyRegistered = true;

        emit PrivacyRegistrationFinalized(admin, _pendingSlots.length);

        // Clear pending slots to save gas on future storage operations
        delete _pendingSlots;
    }

    /// @notice Convenience overload - uses msg.sender as admin, doesn't hide events
    /// @dev Most common usage pattern
    function _finalizePrivacyRegistration() internal {
        _finalizePrivacyRegistration(msg.sender, false);
    }

    /*//////////////////////////////////////////////////////////////
                          AUTHORIZATION HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Grant read access to a private slot
    /// @dev Caller must be the data owner for this slot
    /// @param slot The specific storage slot (computed, not base slot)
    /// @param grantee Address to grant access to
    function _grantReadAccess(uint256 slot, address grantee) internal {
        (bool success, ) = PRIVACY_AUTH.call(
            abi.encodeWithSelector(
                IPrivacyAuth.grant.selector,
                address(this),
                slot,
                grantee,
                Permission.READ
            )
        );
        require(success, "PrivacyEnabled: grant failed");
    }

    /// @notice Grant write access to a private slot
    /// @param slot The specific storage slot
    /// @param grantee Address to grant access to
    function _grantWriteAccess(uint256 slot, address grantee) internal {
        (bool success, ) = PRIVACY_AUTH.call(
            abi.encodeWithSelector(
                IPrivacyAuth.grant.selector,
                address(this),
                slot,
                grantee,
                Permission.WRITE
            )
        );
        require(success, "PrivacyEnabled: grant failed");
    }

    /// @notice Grant full read/write access to a private slot
    /// @param slot The specific storage slot
    /// @param grantee Address to grant access to
    function _grantFullAccess(uint256 slot, address grantee) internal {
        (bool success, ) = PRIVACY_AUTH.call(
            abi.encodeWithSelector(
                IPrivacyAuth.grant.selector,
                address(this),
                slot,
                grantee,
                Permission.READ_WRITE
            )
        );
        require(success, "PrivacyEnabled: grant failed");
    }

    /// @notice Revoke all access from a private slot
    /// @param slot The specific storage slot
    /// @param grantee Address to revoke access from
    function _revokeAccess(uint256 slot, address grantee) internal {
        (bool success, ) = PRIVACY_AUTH.call(
            abi.encodeWithSelector(
                IPrivacyAuth.revoke.selector,
                address(this),
                slot,
                grantee
            )
        );
        require(success, "PrivacyEnabled: revoke failed");
    }

    /*//////////////////////////////////////////////////////////////
                         SLOT CALCULATION HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Calculate the storage slot for a mapping entry
    /// @dev Use this to get the actual slot to pass to grant/revoke
    /// @param key The mapping key
    /// @param baseSlot The mapping's base slot number
    /// @return slot The computed storage slot
    function _computeMappingSlot(address key, uint256 baseSlot) internal pure returns (uint256) {
        return PrivacyLib.mappingSlot(key, baseSlot);
    }

    /// @notice Calculate the storage slot for a nested mapping entry
    /// @param outerKey The outer mapping key
    /// @param innerKey The inner mapping key
    /// @param baseSlot The mapping's base slot number
    /// @return slot The computed storage slot
    function _computeNestedMappingSlot(
        address outerKey,
        address innerKey,
        uint256 baseSlot
    ) internal pure returns (uint256) {
        return PrivacyLib.nestedMappingSlot(outerKey, innerKey, baseSlot);
    }

    /*//////////////////////////////////////////////////////////////
                              VIEW FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if this contract is registered for privacy
    /// @return registered True if privacy registration is finalized
    function isPrivacyEnabled() public view returns (bool) {
        return _privacyRegistered;
    }

    /// @notice Get the privacy admin address
    /// @return admin The admin address (can modify privacy settings)
    function privacyAdmin() public view returns (address) {
        return _privacyAdmin;
    }
}
