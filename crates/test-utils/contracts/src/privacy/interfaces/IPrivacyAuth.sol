// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title IPrivacyAuth
/// @author Base Privacy Team
/// @notice Interface for the Privacy Authorization precompile at 0x0201
/// @dev Allows slot owners to grant and revoke access to their private data
///
/// @dev Example usage:
/// ```solidity
/// IPrivacyAuth auth = IPrivacyAuth(PRIVACY_AUTH);
///
/// // Grant read access to a specific slot
/// auth.grant(myContract, slotNumber, grantee, Permission.READ);
///
/// // Check if someone is authorized
/// bool canRead = auth.isAuthorized(myContract, slotNumber, query);
///
/// // Revoke access
/// auth.revoke(myContract, slotNumber, grantee);
/// ```
///
/// @custom:address 0x0000000000000000000000000000000000000201
/// @custom:gas-grant 3000
/// @custom:gas-revoke 3000
/// @custom:gas-query 100
interface IPrivacyAuth {
    /*//////////////////////////////////////////////////////////////
                                 ERRORS
    //////////////////////////////////////////////////////////////*/

    /// @notice Thrown when caller is not the slot owner
    error NotSlotOwner();

    /// @notice Thrown when grantee address is invalid (zero address)
    error InvalidGrantee();

    /// @notice Thrown when permission value is invalid (must be 1, 2, or 3)
    error InvalidPermission();

    /// @notice Thrown when the contract is not registered for privacy
    error ContractNotRegistered();

    /// @notice Thrown when the precompile call fails
    error PrecompileCallFailed();

    /*//////////////////////////////////////////////////////////////
                                 EVENTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Emitted when access is granted to a slot
    /// @param contract_ The contract containing the private slot
    /// @param slot The storage slot (or 0 for wildcard)
    /// @param grantee The address receiving access
    /// @param permission The permission level granted (READ=1, WRITE=2, READ_WRITE=3)
    event AccessGranted(
        address indexed contract_,
        uint256 indexed slot,
        address indexed grantee,
        uint8 permission
    );

    /// @notice Emitted when access is revoked from a slot
    /// @param contract_ The contract containing the private slot
    /// @param slot The storage slot (or 0 for wildcard)
    /// @param grantee The address losing access
    event AccessRevoked(
        address indexed contract_,
        uint256 indexed slot,
        address indexed grantee
    );

    /*//////////////////////////////////////////////////////////////
                          AUTHORIZATION FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Grant access to a private storage slot
    /// @dev Only callable by the slot owner (determined by the slot's OwnershipType)
    /// @dev For wildcard access to all slots, use slot = 0
    /// @dev If the grantee already has permissions, they are OR'd together (merged)
    /// @dev Gas cost: ~3000
    /// @param contract_ The contract containing the private slot
    /// @param slot The specific slot to grant access to (0 for all slots)
    /// @param grantee The address to grant access to
    /// @param permission Permission level: READ (0x01), WRITE (0x02), or READ_WRITE (0x03)
    function grant(
        address contract_,
        uint256 slot,
        address grantee,
        uint8 permission
    ) external;

    /// @notice Revoke all access from a private storage slot
    /// @dev Only callable by the slot owner
    /// @dev Completely removes the authorization - to reduce permissions, grant with lower value
    /// @dev Gas cost: ~3000
    /// @param contract_ The contract containing the private slot
    /// @param slot The specific slot to revoke access from (0 for wildcard)
    /// @param grantee The address to revoke access from
    function revoke(
        address contract_,
        uint256 slot,
        address grantee
    ) external;

    /*//////////////////////////////////////////////////////////////
                              VIEW FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if an address is authorized to access a slot
    /// @dev Checks for READ permission specifically
    /// @dev Returns true if the query address is the slot owner OR has explicit authorization
    /// @dev Gas cost: ~100
    /// @param contract_ The contract containing the private slot
    /// @param slot The specific slot to check
    /// @param query The address to check authorization for
    /// @return authorized True if the address has READ permission
    function isAuthorized(
        address contract_,
        uint256 slot,
        address query
    ) external view returns (bool authorized);
}
