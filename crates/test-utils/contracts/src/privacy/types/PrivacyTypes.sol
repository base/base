// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Privacy Types
/// @author Base Privacy Team
/// @notice Shared type definitions for privacy precompiles
/// @dev Used by IPrivacyRegistry, IPrivacyAuth, and consuming contracts

/// @notice How a storage slot is organized
/// @dev Must match the Rust SlotType enum in privacy/src/registry.rs
enum SlotType {
    /// Single storage slot (e.g., uint256 secretValue, address admin)
    Simple,
    /// Flat mapping (e.g., mapping(address => uint256) balances)
    Mapping,
    /// Two-level nested mapping (e.g., mapping(address => mapping(address => uint256)) allowances)
    NestedMapping,
    /// Mapping to a struct with consecutive slots
    MappingToStruct
}

/// @notice How ownership of private data is determined
/// @dev Must match the Rust OwnershipType enum in privacy/src/registry.rs
enum OwnershipType {
    /// Contract itself owns all slots (only accessible during execution)
    Contract,
    /// The mapping key (must be an address) is the owner of their entry
    MappingKey,
    /// For nested mappings, the outer key is the owner
    OuterKey,
    /// For nested mappings, the inner key is the owner
    InnerKey
}

/// @notice Permission constants for authorization
/// @dev Bitmask values that can be OR'd together
library Permission {
    /// @notice Read-only access to private data
    uint8 internal constant READ = 0x01;
    /// @notice Write access to private data
    uint8 internal constant WRITE = 0x02;
    /// @notice Both read and write access
    uint8 internal constant READ_WRITE = 0x03;
}

/// @notice Configuration for a private storage slot or slot family
/// @param baseSlot The base slot number (before key hashing for mappings)
/// @param slotType Type of slot structure (Simple, Mapping, etc.)
/// @param ownership How ownership is determined for this slot
struct SlotConfig {
    uint256 baseSlot;
    SlotType slotType;
    OwnershipType ownership;
}

/// @dev Address of the Privacy Registry precompile (0x0200)
/// Handles contract registration and slot configuration
address constant PRIVACY_REGISTRY = 0x0000000000000000000000000000000000000200;

/// @dev Address of the Privacy Auth precompile (0x0201)
/// Handles authorization grants and queries
address constant PRIVACY_AUTH = 0x0000000000000000000000000000000000000201;
