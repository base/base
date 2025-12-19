//! Slot Classification
//!
//! Determines whether a storage slot is public or private based on
//! the privacy registry configuration.

use crate::registry::{OwnershipType, PrivacyRegistry, SlotType};
use alloy_primitives::{Address, U256};

/// Classification of a storage slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotClassification {
    /// The slot is public and stored in the standard state trie.
    Public,
    /// The slot is private and stored in the TEE-local store.
    Private {
        /// The owner of this slot (for authorization purposes)
        owner: Address,
    },
}

impl SlotClassification {
    /// Returns `true` if the slot is private.
    pub fn is_private(&self) -> bool {
        matches!(self, Self::Private { .. })
    }

    /// Returns `true` if the slot is public.
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public)
    }

    /// Returns the owner if the slot is private.
    pub fn owner(&self) -> Option<Address> {
        match self {
            Self::Private { owner } => Some(*owner),
            Self::Public => None,
        }
    }
}

/// Classify a storage slot as public or private.
///
/// # Arguments
///
/// * `registry` - The privacy registry to consult
/// * `contract` - The contract address
/// * `slot` - The storage slot being accessed
///
/// # Returns
///
/// - `SlotClassification::Public` if the contract is not registered or the slot is not private
/// - `SlotClassification::Private { owner }` if the slot is private
///
/// # Example
///
/// ```ignore
/// use base_reth_privacy::{PrivacyRegistry, classify_slot, SlotClassification};
///
/// let registry = PrivacyRegistry::new();
/// // ... register contracts ...
///
/// match classify_slot(&registry, contract, slot) {
///     SlotClassification::Public => {
///         // Read from triedb
///     }
///     SlotClassification::Private { owner } => {
///         // Read from private store
///     }
/// }
/// ```
pub fn classify_slot(registry: &PrivacyRegistry, contract: Address, slot: U256) -> SlotClassification {
    // Fast path: unregistered contracts are always public
    let config = match registry.get_config(&contract) {
        Some(c) => c,
        None => return SlotClassification::Public,
    };

    // Check each slot configuration
    for slot_config in &config.slots {
        match slot_config.slot_type {
            SlotType::Simple => {
                // Simple slots: exact match
                if slot == slot_config.base_slot {
                    let owner = resolve_simple_owner(&slot_config.ownership, contract);
                    return SlotClassification::Private { owner };
                }
            }
            SlotType::Mapping | SlotType::NestedMapping | SlotType::MappingToStruct { .. } => {
                // For mappings, we can't reverse the hash to check the base slot.
                // Instead, check if this slot was recorded as a mapping slot.
                if let Some(owner) = registry.get_slot_owner(contract, slot) {
                    return SlotClassification::Private { owner };
                }
            }
        }
    }

    // Default: public
    SlotClassification::Public
}

/// Resolve the owner for a simple (non-mapping) slot.
fn resolve_simple_owner(ownership: &OwnershipType, contract: Address) -> Address {
    match ownership {
        OwnershipType::Contract => contract,
        OwnershipType::FixedOwner(addr) => *addr,
        // For simple slots, these don't really apply but we handle them
        OwnershipType::MappingKey
        | OwnershipType::OuterKey
        | OwnershipType::InnerKey => contract,
    }
}

/// Resolve the owner for a mapping slot based on the key used.
///
/// This is called when we intercept a mapping write and need to determine
/// who owns the resulting slot.
///
/// # Arguments
///
/// * `ownership` - How ownership is determined for this mapping
/// * `key` - The mapping key (typically an address)
/// * `outer_key` - For nested mappings, the outer key
/// * `contract` - The contract address (fallback owner)
pub fn resolve_mapping_owner(
    ownership: &OwnershipType,
    key: Address,
    outer_key: Option<Address>,
    contract: Address,
) -> Address {
    match ownership {
        OwnershipType::Contract => contract,
        OwnershipType::MappingKey => key,
        OwnershipType::OuterKey => outer_key.unwrap_or(key),
        OwnershipType::InnerKey => key,
        OwnershipType::FixedOwner(addr) => *addr,
    }
}

/// Compute the storage slot for a mapping access.
///
/// For `mapping(address => T)`, the slot is `keccak256(abi.encode(key, base_slot))`.
///
/// # Arguments
///
/// * `base_slot` - The base slot of the mapping
/// * `key` - The mapping key
///
/// # Returns
///
/// The computed storage slot.
pub fn compute_mapping_slot(base_slot: U256, key: Address) -> U256 {
    use alloy_primitives::keccak256;

    // abi.encode(key, base_slot) = 32 bytes for key (left-padded) + 32 bytes for slot
    let mut data = [0u8; 64];
    // Key is right-aligned in first 32 bytes (address is 20 bytes)
    data[12..32].copy_from_slice(key.as_slice());
    // Slot is big-endian in second 32 bytes
    data[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());

    let hash = keccak256(&data);
    U256::from_be_bytes(hash.0)
}

/// Compute the storage slot for a nested mapping access.
///
/// For `mapping(address => mapping(address => T))`, the slot is:
/// `keccak256(abi.encode(inner_key, keccak256(abi.encode(outer_key, base_slot))))`
///
/// # Arguments
///
/// * `base_slot` - The base slot of the outer mapping
/// * `outer_key` - The key for the outer mapping
/// * `inner_key` - The key for the inner mapping
///
/// # Returns
///
/// The computed storage slot.
pub fn compute_nested_mapping_slot(base_slot: U256, outer_key: Address, inner_key: Address) -> U256 {
    let outer_slot = compute_mapping_slot(base_slot, outer_key);
    compute_mapping_slot(outer_slot, inner_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PrivateContractConfig, SlotConfig};

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    #[test]
    fn test_unregistered_contract_is_public() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let slot = U256::from(0);

        let result = classify_slot(&registry, contract, slot);
        assert!(result.is_public());
        assert_eq!(result.owner(), None);
    }

    #[test]
    fn test_simple_private_slot() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);

        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            }],
            registered_at: 1,
            hide_events: false,
        };

        registry.register(config).unwrap();

        let result = classify_slot(&registry, contract, U256::from(5));
        assert!(result.is_private());
        assert_eq!(result.owner(), Some(contract));
    }

    #[test]
    fn test_simple_slot_with_fixed_owner() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);
        let fixed_owner = test_address(99);

        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::FixedOwner(fixed_owner),
            }],
            registered_at: 1,
            hide_events: false,
        };

        registry.register(config).unwrap();

        let result = classify_slot(&registry, contract, U256::from(5));
        assert!(result.is_private());
        assert_eq!(result.owner(), Some(fixed_owner));
    }

    #[test]
    fn test_non_registered_slot_is_public() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);

        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            }],
            registered_at: 1,
            hide_events: false,
        };

        registry.register(config).unwrap();

        // Slot 10 is not registered as private
        let result = classify_slot(&registry, contract, U256::from(10));
        assert!(result.is_public());
    }

    #[test]
    fn test_mapping_slot_with_recorded_owner() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);

        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::ZERO,
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            }],
            registered_at: 1,
            hide_events: false,
        };

        registry.register(config).unwrap();

        // Compute the slot for balances[alice]
        let computed_slot = compute_mapping_slot(U256::ZERO, alice);

        // Record the owner (this would happen during SSTORE interception)
        registry.record_slot_owner(contract, computed_slot, alice);

        // Now classify the slot
        let result = classify_slot(&registry, contract, computed_slot);
        assert!(result.is_private());
        assert_eq!(result.owner(), Some(alice));
    }

    #[test]
    fn test_unrecorded_mapping_slot_is_public() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);

        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::ZERO,
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            }],
            registered_at: 1,
            hide_events: false,
        };

        registry.register(config).unwrap();

        // Compute the slot for balances[alice] but DON'T record the owner
        let computed_slot = compute_mapping_slot(U256::ZERO, alice);

        // Without recording, we can't know it's private
        let result = classify_slot(&registry, contract, computed_slot);
        assert!(result.is_public());
    }

    #[test]
    fn test_compute_mapping_slot() {
        let base_slot = U256::ZERO;
        let key = Address::new([0xAB; 20]);

        let slot1 = compute_mapping_slot(base_slot, key);
        let slot2 = compute_mapping_slot(base_slot, key);

        // Same inputs produce same output
        assert_eq!(slot1, slot2);

        // Different key produces different slot
        let other_key = Address::new([0xCD; 20]);
        let slot3 = compute_mapping_slot(base_slot, other_key);
        assert_ne!(slot1, slot3);

        // Different base slot produces different slot
        let slot4 = compute_mapping_slot(U256::from(1), key);
        assert_ne!(slot1, slot4);
    }

    #[test]
    fn test_compute_nested_mapping_slot() {
        let base_slot = U256::from(1); // allowances
        let owner = Address::new([0xAA; 20]);
        let spender = Address::new([0xBB; 20]);

        let slot = compute_nested_mapping_slot(base_slot, owner, spender);

        // Verify it's different from a simple mapping
        let simple_slot = compute_mapping_slot(base_slot, owner);
        assert_ne!(slot, simple_slot);

        // Verify order matters
        let reversed = compute_nested_mapping_slot(base_slot, spender, owner);
        assert_ne!(slot, reversed);
    }

    #[test]
    fn test_resolve_mapping_owner() {
        let key = test_address(10);
        let outer_key = test_address(20);
        let contract = test_address(1);
        let fixed = test_address(99);

        assert_eq!(
            resolve_mapping_owner(&OwnershipType::Contract, key, None, contract),
            contract
        );
        assert_eq!(
            resolve_mapping_owner(&OwnershipType::MappingKey, key, None, contract),
            key
        );
        assert_eq!(
            resolve_mapping_owner(&OwnershipType::OuterKey, key, Some(outer_key), contract),
            outer_key
        );
        assert_eq!(
            resolve_mapping_owner(&OwnershipType::InnerKey, key, Some(outer_key), contract),
            key
        );
        assert_eq!(
            resolve_mapping_owner(&OwnershipType::FixedOwner(fixed), key, None, contract),
            fixed
        );
    }

    #[test]
    fn test_slot_classification_methods() {
        let public = SlotClassification::Public;
        assert!(public.is_public());
        assert!(!public.is_private());
        assert_eq!(public.owner(), None);

        let owner = test_address(10);
        let private = SlotClassification::Private { owner };
        assert!(private.is_private());
        assert!(!private.is_public());
        assert_eq!(private.owner(), Some(owner));
    }

    #[test]
    fn test_multiple_slot_configs() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);

        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![
                SlotConfig {
                    base_slot: U256::from(0),
                    slot_type: SlotType::Mapping,
                    ownership: OwnershipType::MappingKey,
                },
                SlotConfig {
                    base_slot: U256::from(1),
                    slot_type: SlotType::NestedMapping,
                    ownership: OwnershipType::OuterKey,
                },
                SlotConfig {
                    base_slot: U256::from(5),
                    slot_type: SlotType::Simple,
                    ownership: OwnershipType::Contract,
                },
            ],
            registered_at: 1,
            hide_events: false,
        };

        registry.register(config).unwrap();

        // Simple slot should be private
        assert!(classify_slot(&registry, contract, U256::from(5)).is_private());

        // Simple slot 6 should be public (not in config)
        assert!(classify_slot(&registry, contract, U256::from(6)).is_public());

        // Record a mapping slot owner
        let balance_slot = compute_mapping_slot(U256::ZERO, alice);
        registry.record_slot_owner(contract, balance_slot, alice);
        assert!(classify_slot(&registry, contract, balance_slot).is_private());
    }
}
