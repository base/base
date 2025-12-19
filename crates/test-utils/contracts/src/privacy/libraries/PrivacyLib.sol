// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {SlotConfig, SlotType, OwnershipType} from "../types/PrivacyTypes.sol";

/// @title PrivacyLib
/// @author Base Privacy Team
/// @notice Utility library for privacy operations
/// @dev All functions are internal pure for zero gas overhead (inlined at compile time)
///
/// @dev This library provides:
/// - Slot calculation helpers (mirrors Solidity's storage layout)
/// - SlotConfig builders for common patterns
/// - Validation functions
library PrivacyLib {
    /*//////////////////////////////////////////////////////////////
                           SLOT CALCULATIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Calculate the storage slot for a mapping entry
    /// @dev Mirrors Solidity's mapping slot calculation: keccak256(abi.encode(key, baseSlot))
    /// @param key The mapping key (typically an address)
    /// @param baseSlot The mapping's base storage slot
    /// @return slot The computed storage slot where the value is stored
    function mappingSlot(address key, uint256 baseSlot) internal pure returns (uint256) {
        return uint256(keccak256(abi.encode(key, baseSlot)));
    }

    /// @notice Calculate the storage slot for a nested mapping entry
    /// @dev For mapping(outer => mapping(inner => value))
    /// @dev Calculation: keccak256(abi.encode(inner, keccak256(abi.encode(outer, baseSlot))))
    /// @param outerKey The outer mapping key
    /// @param innerKey The inner mapping key
    /// @param baseSlot The mapping's base storage slot
    /// @return slot The computed storage slot
    function nestedMappingSlot(
        address outerKey,
        address innerKey,
        uint256 baseSlot
    ) internal pure returns (uint256) {
        uint256 intermediate = uint256(keccak256(abi.encode(outerKey, baseSlot)));
        return uint256(keccak256(abi.encode(innerKey, intermediate)));
    }

    /// @notice Calculate slot for a uint256 key in a mapping
    /// @dev Useful for mappings like mapping(uint256 => X)
    /// @param key The uint256 mapping key
    /// @param baseSlot The mapping's base storage slot
    /// @return slot The computed storage slot
    function mappingSlotUint(uint256 key, uint256 baseSlot) internal pure returns (uint256) {
        return uint256(keccak256(abi.encode(key, baseSlot)));
    }

    /*//////////////////////////////////////////////////////////////
                          CONFIG BUILDERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Create a SlotConfig for a simple private slot
    /// @dev Use for single storage slots like: uint256 private _secret;
    /// @dev The contract owns this slot (no per-user ownership)
    /// @param slot The storage slot number
    /// @return config The slot configuration
    function simpleSlot(uint256 slot) internal pure returns (SlotConfig memory) {
        return SlotConfig({
            baseSlot: slot,
            slotType: SlotType.Simple,
            ownership: OwnershipType.Contract
        });
    }

    /// @notice Create a SlotConfig for a mapping where keys own their data
    /// @dev Use for: mapping(address => X) where each address owns their entry
    /// @dev Example: mapping(address => uint256) private _balances;
    /// @param slot The mapping's base storage slot
    /// @return config The slot configuration
    function mappingKeyOwned(uint256 slot) internal pure returns (SlotConfig memory) {
        return SlotConfig({
            baseSlot: slot,
            slotType: SlotType.Mapping,
            ownership: OwnershipType.MappingKey
        });
    }

    /// @notice Create a SlotConfig for a contract-owned mapping
    /// @dev Use when the contract should control all entries, not individual users
    /// @param slot The mapping's base storage slot
    /// @return config The slot configuration
    function mappingContractOwned(uint256 slot) internal pure returns (SlotConfig memory) {
        return SlotConfig({
            baseSlot: slot,
            slotType: SlotType.Mapping,
            ownership: OwnershipType.Contract
        });
    }

    /// @notice Create a SlotConfig for an allowances-style nested mapping
    /// @dev Use for: mapping(owner => mapping(spender => amount))
    /// @dev The outer key (owner) controls visibility of all their allowances
    /// @param slot The mapping's base storage slot
    /// @return config The slot configuration
    function allowancesSlot(uint256 slot) internal pure returns (SlotConfig memory) {
        return SlotConfig({
            baseSlot: slot,
            slotType: SlotType.NestedMapping,
            ownership: OwnershipType.OuterKey
        });
    }

    /// @notice Create a SlotConfig for a delegation-style nested mapping
    /// @dev Use for: mapping(delegator => mapping(delegate => data))
    /// @dev The inner key (delegate) owns visibility of their delegation data
    /// @param slot The mapping's base storage slot
    /// @return config The slot configuration
    function delegationSlot(uint256 slot) internal pure returns (SlotConfig memory) {
        return SlotConfig({
            baseSlot: slot,
            slotType: SlotType.NestedMapping,
            ownership: OwnershipType.InnerKey
        });
    }

    /*//////////////////////////////////////////////////////////////
                           VALIDATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Validate a SlotConfig for logical consistency
    /// @dev Checks that the ownership type makes sense for the slot type
    /// @param config The configuration to validate
    /// @return valid True if the configuration is valid
    function isValid(SlotConfig memory config) internal pure returns (bool) {
        // Simple slots should typically have Contract ownership
        if (config.slotType == SlotType.Simple) {
            return config.ownership == OwnershipType.Contract;
        }

        // Mappings can have MappingKey or Contract ownership
        if (config.slotType == SlotType.Mapping) {
            return
                config.ownership == OwnershipType.MappingKey ||
                config.ownership == OwnershipType.Contract;
        }

        // Nested mappings can have OuterKey, InnerKey, or Contract ownership
        if (config.slotType == SlotType.NestedMapping) {
            return
                config.ownership == OwnershipType.OuterKey ||
                config.ownership == OwnershipType.InnerKey ||
                config.ownership == OwnershipType.Contract;
        }

        // MappingToStruct can have MappingKey or Contract ownership
        if (config.slotType == SlotType.MappingToStruct) {
            return
                config.ownership == OwnershipType.MappingKey ||
                config.ownership == OwnershipType.Contract;
        }

        return true;
    }

    /*//////////////////////////////////////////////////////////////
                              HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Get the number of words a SlotConfig occupies when ABI encoded
    /// @dev Useful for gas estimation
    /// @return words Number of 32-byte words (always 3 for SlotConfig)
    function slotConfigWords() internal pure returns (uint256) {
        return 3; // baseSlot (32) + slotType (32) + ownership (32)
    }
}
