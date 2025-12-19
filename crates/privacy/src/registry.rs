//! Privacy Registry
//!
//! The registry maintains configurations for contracts that have private storage slots.
//! It tracks which contracts are registered, their slot configurations, and the
//! mapping from computed slots back to their owners.

use alloy_primitives::{Address, U256};
use derive_more::Display;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Configuration for a registered private contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateContractConfig {
    /// Contract address
    pub address: Address,
    /// Who can update this config (deployer or designated admin)
    pub admin: Address,
    /// Slot configurations
    pub slots: Vec<SlotConfig>,
    /// When registered (block number)
    pub registered_at: u64,
    /// Whether to hide events from this contract
    pub hide_events: bool,
}

/// Configuration for a single storage slot or slot family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotConfig {
    /// Base slot number (for mappings, this is the slot before key hashing)
    pub base_slot: U256,
    /// Type of slot
    pub slot_type: SlotType,
    /// How ownership is determined
    pub ownership: OwnershipType,
}

/// Type of storage slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotType {
    /// Single storage slot (e.g., `uint256 private secretValue`)
    Simple,
    /// Mapping (e.g., `mapping(address => uint256) balances`)
    Mapping,
    /// Nested mapping (e.g., `mapping(address => mapping(address => uint256))`)
    NestedMapping,
    /// Struct in a mapping (multiple consecutive slots per key)
    MappingToStruct {
        /// Number of slots in the struct
        struct_size: u8,
    },
}

/// How ownership of a slot is determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipType {
    /// Contract itself owns all slots (only contract can read via execution)
    Contract,
    /// The mapping key (must be address) is the owner
    /// e.g., balances[alice] is owned by alice
    MappingKey,
    /// For nested mappings, outer key is owner
    /// e.g., allowances[owner][spender] — owner is the owner
    OuterKey,
    /// For nested mappings, inner key is owner
    /// e.g., delegations[delegator][delegate] — delegate is the owner
    InnerKey,
    /// Specific address owns all slots
    FixedOwner(Address),
}

/// Errors that can occur during registry operations.
#[derive(Debug, Display, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// Contract is already registered
    #[display("contract {_0} is already registered")]
    AlreadyRegistered(Address),

    /// Contract is not registered
    #[display("contract {_0} is not registered")]
    NotRegistered(Address),

    /// Caller is not authorized to perform this action
    #[display("caller {caller} is not authorized (admin is {admin})")]
    NotAuthorized {
        /// The caller's address
        caller: Address,
        /// The admin's address
        admin: Address,
    },

    /// Invalid slot configuration
    #[display("invalid slot configuration: {_0}")]
    InvalidConfig(String),

    /// Internal lock error
    #[display("internal lock error")]
    LockError,
}

impl std::error::Error for RegistryError {}

/// The Privacy Registry manages contract registrations and slot configurations.
///
/// Thread-safe via internal `RwLock` - can be cloned and shared across threads.
#[derive(Debug, Clone)]
pub struct PrivacyRegistry {
    /// Contract configurations: address → config
    configs: Arc<RwLock<HashMap<Address, PrivateContractConfig>>>,
    /// Mapping from computed slot → owner for mapping types
    /// Key: (contract, computed_slot) → owner address
    slot_owners: Arc<RwLock<HashMap<(Address, U256), Address>>>,
}

impl Default for PrivacyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PrivacyRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            configs: Arc::new(RwLock::new(HashMap::new())),
            slot_owners: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new contract for private storage.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is already registered.
    pub fn register(&self, config: PrivateContractConfig) -> Result<(), RegistryError> {
        let mut configs = self.write_configs()?;

        if configs.contains_key(&config.address) {
            return Err(RegistryError::AlreadyRegistered(config.address));
        }

        configs.insert(config.address, config);
        Ok(())
    }

    /// Update the slot configuration for a registered contract.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is not registered or the caller is not the admin.
    pub fn update_slots(
        &self,
        contract: Address,
        slots: Vec<SlotConfig>,
        caller: Address,
    ) -> Result<(), RegistryError> {
        let mut configs = self.write_configs()?;

        let config = configs
            .get_mut(&contract)
            .ok_or(RegistryError::NotRegistered(contract))?;

        if config.admin != caller {
            return Err(RegistryError::NotAuthorized {
                caller,
                admin: config.admin,
            });
        }

        config.slots = slots;
        Ok(())
    }

    /// Add slots to an existing contract registration.
    ///
    /// Unlike `update_slots`, this appends to the existing slots rather than replacing them.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is not registered.
    pub fn add_slots(
        &self,
        contract: &Address,
        new_slots: Vec<SlotConfig>,
    ) -> Result<(), RegistryError> {
        let mut configs = self.write_configs()?;

        let config = configs
            .get_mut(contract)
            .ok_or(RegistryError::NotRegistered(*contract))?;

        config.slots.extend(new_slots);
        Ok(())
    }

    /// Set the hide_events flag for a contract.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is not registered.
    pub fn set_hide_events(&self, contract: &Address, hide: bool) -> Result<(), RegistryError> {
        let mut configs = self.write_configs()?;

        let config = configs
            .get_mut(contract)
            .ok_or(RegistryError::NotRegistered(*contract))?;

        config.hide_events = hide;
        Ok(())
    }

    /// Transfer admin rights for a contract.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is not registered or the caller is not the current admin.
    pub fn transfer_admin(
        &self,
        contract: Address,
        new_admin: Address,
        caller: Address,
    ) -> Result<(), RegistryError> {
        let mut configs = self.write_configs()?;

        let config = configs
            .get_mut(&contract)
            .ok_or(RegistryError::NotRegistered(contract))?;

        if config.admin != caller {
            return Err(RegistryError::NotAuthorized {
                caller,
                admin: config.admin,
            });
        }

        config.admin = new_admin;
        Ok(())
    }

    /// Check if a contract is registered.
    pub fn is_registered(&self, address: &Address) -> bool {
        self.read_configs()
            .map(|configs| configs.contains_key(address))
            .unwrap_or(false)
    }

    /// Get the configuration for a contract.
    pub fn get_config(&self, address: &Address) -> Option<PrivateContractConfig> {
        self.read_configs()
            .ok()
            .and_then(|configs| configs.get(address).cloned())
    }

    /// Record the owner of a computed mapping slot.
    ///
    /// Called during SSTORE when we detect a write to a mapping slot.
    pub fn record_slot_owner(&self, contract: Address, slot: U256, owner: Address) {
        if let Ok(mut slot_owners) = self.slot_owners.write() {
            slot_owners.insert((contract, slot), owner);
        }
    }

    /// Get the owner of a computed mapping slot.
    pub fn get_slot_owner(&self, contract: Address, slot: U256) -> Option<Address> {
        self.slot_owners
            .read()
            .ok()
            .and_then(|owners| owners.get(&(contract, slot)).copied())
    }

    /// Check if a slot is configured as private for a contract.
    ///
    /// For simple slots, this checks if the exact slot matches.
    /// For mapping slots, this checks the slot_owners cache.
    pub fn is_slot_private(&self, contract: &Address, slot: &U256) -> bool {
        let config = match self.get_config(contract) {
            Some(c) => c,
            None => return false,
        };

        // Check simple slots
        for slot_config in &config.slots {
            if slot_config.slot_type == SlotType::Simple && &slot_config.base_slot == slot {
                return true;
            }
        }

        // Check if it's a known mapping slot
        self.get_slot_owner(*contract, *slot).is_some()
    }

    /// Get all registered contracts.
    pub fn registered_contracts(&self) -> Vec<Address> {
        self.read_configs()
            .map(|configs| configs.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Get the total number of registered contracts.
    pub fn len(&self) -> usize {
        self.read_configs().map(|c| c.len()).unwrap_or(0)
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // Internal helpers

    fn read_configs(
        &self,
    ) -> Result<RwLockReadGuard<'_, HashMap<Address, PrivateContractConfig>>, RegistryError> {
        self.configs.read().map_err(|_| RegistryError::LockError)
    }

    fn write_configs(
        &self,
    ) -> Result<RwLockWriteGuard<'_, HashMap<Address, PrivateContractConfig>>, RegistryError> {
        self.configs.write().map_err(|_| RegistryError::LockError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn test_config(address: Address, admin: Address) -> PrivateContractConfig {
        PrivateContractConfig {
            address,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::ZERO,
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            }],
            registered_at: 1,
            hide_events: false,
        }
    }

    #[test]
    fn test_new_registry_is_empty() {
        let registry = PrivacyRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_register_contract() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);

        let config = test_config(contract, admin);
        registry.register(config.clone()).unwrap();

        assert!(registry.is_registered(&contract));
        assert_eq!(registry.len(), 1);

        let retrieved = registry.get_config(&contract).unwrap();
        assert_eq!(retrieved.address, contract);
        assert_eq!(retrieved.admin, admin);
    }

    #[test]
    fn test_register_duplicate_fails() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);

        let config = test_config(contract, admin);
        registry.register(config.clone()).unwrap();

        let result = registry.register(config);
        assert!(matches!(result, Err(RegistryError::AlreadyRegistered(_))));
    }

    #[test]
    fn test_update_slots_by_admin() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);

        registry.register(test_config(contract, admin)).unwrap();

        let new_slots = vec![
            SlotConfig {
                base_slot: U256::ZERO,
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            },
            SlotConfig {
                base_slot: U256::from(1),
                slot_type: SlotType::NestedMapping,
                ownership: OwnershipType::OuterKey,
            },
        ];

        registry.update_slots(contract, new_slots, admin).unwrap();

        let config = registry.get_config(&contract).unwrap();
        assert_eq!(config.slots.len(), 2);
    }

    #[test]
    fn test_update_slots_by_non_admin_fails() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);
        let attacker = test_address(3);

        registry.register(test_config(contract, admin)).unwrap();

        let new_slots = vec![];
        let result = registry.update_slots(contract, new_slots, attacker);

        assert!(matches!(result, Err(RegistryError::NotAuthorized { .. })));
    }

    #[test]
    fn test_transfer_admin() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);
        let new_admin = test_address(3);

        registry.register(test_config(contract, admin)).unwrap();
        registry.transfer_admin(contract, new_admin, admin).unwrap();

        let config = registry.get_config(&contract).unwrap();
        assert_eq!(config.admin, new_admin);
    }

    #[test]
    fn test_slot_owner_tracking() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let alice = test_address(10);
        let computed_slot = U256::from(0x1234u64);

        registry.record_slot_owner(contract, computed_slot, alice);

        assert_eq!(
            registry.get_slot_owner(contract, computed_slot),
            Some(alice)
        );
        assert_eq!(
            registry.get_slot_owner(contract, U256::from(0x5678u64)),
            None
        );
    }

    #[test]
    fn test_is_slot_private_simple() {
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

        assert!(registry.is_slot_private(&contract, &U256::from(5)));
        assert!(!registry.is_slot_private(&contract, &U256::from(6)));
    }

    #[test]
    fn test_is_slot_private_mapping() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let computed_slot = U256::from(0xABCDu64);

        registry.register(test_config(contract, admin)).unwrap();
        registry.record_slot_owner(contract, computed_slot, alice);

        assert!(registry.is_slot_private(&contract, &computed_slot));
    }

    #[test]
    fn test_unregistered_contract_not_private() {
        let registry = PrivacyRegistry::new();
        let contract = test_address(1);

        assert!(!registry.is_registered(&contract));
        assert!(!registry.is_slot_private(&contract, &U256::ZERO));
    }

    #[test]
    fn test_registered_contracts_list() {
        let registry = PrivacyRegistry::new();

        let c1 = test_address(1);
        let c2 = test_address(2);
        let c3 = test_address(3);
        let admin = test_address(99);

        registry.register(test_config(c1, admin)).unwrap();
        registry.register(test_config(c2, admin)).unwrap();
        registry.register(test_config(c3, admin)).unwrap();

        let registered = registry.registered_contracts();
        assert_eq!(registered.len(), 3);
        assert!(registered.contains(&c1));
        assert!(registered.contains(&c2));
        assert!(registered.contains(&c3));
    }
}
