//! Privacy-Aware Database Wrapper
//!
//! Wraps the underlying state database to intercept SLOAD/SSTORE operations
//! and route private slots to the TEE-local store.

use crate::{
    classification::{classify_slot, resolve_mapping_owner, SlotClassification},
    inspector::SlotKeyCache,
    registry::PrivacyRegistry,
    store::PrivateStateStore,
};
use alloy_primitives::{Address, B256, U256};
use revm::{
    database_interface::DBErrorMarker,
    primitives::{HashMap, StorageKey, StorageValue},
    state::{Account, AccountInfo, Bytecode, EvmStorageSlot},
    Database, DatabaseCommit, DatabaseRef,
};
use std::sync::Arc;

/// A privacy-aware database wrapper that intercepts storage operations.
///
/// SLOAD operations check if the slot is private and return from the private store.
/// SSTORE operations are intercepted during commit to route private slots appropriately.
#[derive(Debug)]
pub struct PrivacyDatabase<DB> {
    /// The underlying database (typically StateProviderDatabase)
    inner: DB,
    /// Privacy registry for slot classification
    registry: Arc<PrivacyRegistry>,
    /// Private state store for TEE-local storage
    private_store: Arc<PrivateStateStore>,
    /// Current block number for authorization checks
    current_block: u64,
    /// Optional slot key cache from inspector for mapping key resolution
    slot_key_cache: Option<Arc<SlotKeyCache>>,
    /// Transaction sender (used as fallback owner when cache misses)
    tx_sender: Option<Address>,
}

impl<DB> PrivacyDatabase<DB> {
    /// Create a new privacy-aware database wrapper.
    pub fn new(
        inner: DB,
        registry: Arc<PrivacyRegistry>,
        private_store: Arc<PrivateStateStore>,
    ) -> Self {
        Self {
            inner,
            registry,
            private_store,
            current_block: 0,
            slot_key_cache: None,
            tx_sender: None,
        }
    }

    /// Set the slot key cache for mapping key resolution.
    ///
    /// This cache is populated by the privacy inspector during execution
    /// and is used during commit to determine mapping slot ownership.
    pub fn set_slot_key_cache(&mut self, cache: Arc<SlotKeyCache>) {
        self.slot_key_cache = Some(cache);
    }

    /// Set the transaction sender address.
    ///
    /// This is used as a fallback owner when the slot key cache doesn't
    /// contain mapping information for a private slot.
    pub fn set_tx_sender(&mut self, sender: Address) {
        self.tx_sender = Some(sender);
    }

    /// Clear transaction-specific state.
    ///
    /// Should be called between transactions to reset sender and optionally clear cache.
    pub fn clear_tx_context(&mut self) {
        self.tx_sender = None;
        if let Some(cache) = &self.slot_key_cache {
            cache.clear();
        }
    }

    /// Set the current block number for authorization checks.
    pub fn set_block(&mut self, block: u64) {
        self.current_block = block;
        self.private_store.set_block(block);
    }

    /// Get a reference to the underlying database.
    pub fn inner(&self) -> &DB {
        &self.inner
    }

    /// Get a mutable reference to the underlying database.
    pub fn inner_mut(&mut self) -> &mut DB {
        &mut self.inner
    }

    /// Get a reference to the privacy registry.
    pub fn registry(&self) -> &PrivacyRegistry {
        &self.registry
    }

    /// Get a reference to the private store.
    pub fn private_store(&self) -> &PrivateStateStore {
        &self.private_store
    }

    /// Classify a storage slot as public or private.
    fn classify(&self, address: Address, slot: U256) -> SlotClassification {
        classify_slot(&self.registry, address, slot)
    }

    /// Check if a slot value changed and needs to be routed to private store.
    fn is_slot_changed(slot: &EvmStorageSlot) -> bool {
        slot.present_value != slot.original_value
    }

    /// Try to resolve the owner for a slot using the slot key cache.
    ///
    /// If the cache contains mapping key information for this slot, we:
    /// 1. Look up the slot configuration to determine the ownership model
    /// 2. Extract the owner address from the key
    /// 3. Record the owner in the registry for future lookups
    ///
    /// Returns the resolved owner if found, or None if not in cache.
    fn try_resolve_owner_from_cache(&self, contract: Address, slot: U256) -> Option<Address> {
        let cache = self.slot_key_cache.as_ref()?;
        let (base_slot, key) = cache.get(contract, slot)?;

        // Get the slot config for this base slot to determine ownership model
        let config = self.registry.get_config(&contract)?;

        // Find the matching slot configuration by checking base slots
        let slot_config = config.slots.iter().find(|sc| sc.base_slot == base_slot)?;

        // Extract the owner address from the key
        // The key is 32 bytes; for address-keyed mappings, the address is in the last 20 bytes
        let key_address = Address::from_slice(&key[12..32]);

        // Resolve the owner based on the ownership model
        let owner = resolve_mapping_owner(
            &slot_config.ownership,
            key_address,
            None, // TODO: Handle nested mappings with outer key
            contract,
        );

        // Record this owner for future lookups (and for classification to work)
        self.registry.record_slot_owner(contract, slot, owner);

        Some(owner)
    }

    /// Get the fallback owner when no cache information is available.
    ///
    /// Uses the transaction sender if available, otherwise falls back to the contract.
    fn fallback_owner(&self, contract: Address) -> Address {
        self.tx_sender.unwrap_or(contract)
    }

    /// Check if a slot could potentially be a mapping slot that we missed.
    ///
    /// This is a heuristic: if the contract is registered with mapping slots
    /// but we didn't find the slot in our cache, it might still be a mapping
    /// slot (e.g., if the inspector wasn't enabled). In this case, we
    /// conservatively treat it as private with fallback ownership.
    ///
    /// Note: This is disabled by default to avoid false positives. Enable
    /// by setting `tx_sender` to use caller-based fallback ownership.
    fn is_potential_mapping_slot(&self, contract: Address, _slot: U256) -> bool {
        // Only use fallback if tx_sender is set (explicit opt-in)
        if self.tx_sender.is_none() {
            return false;
        }

        // Check if this contract has mapping slots configured
        let config = match self.registry.get_config(&contract) {
            Some(c) => c,
            None => return false,
        };

        // If any mapping slots are configured, non-simple slots might be private
        config.slots.iter().any(|sc| {
            matches!(
                sc.slot_type,
                crate::registry::SlotType::Mapping
                    | crate::registry::SlotType::NestedMapping
                    | crate::registry::SlotType::MappingToStruct { .. }
            )
        })
    }
}

impl<DB: Clone> Clone for PrivacyDatabase<DB> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            registry: Arc::clone(&self.registry),
            private_store: Arc::clone(&self.private_store),
            current_block: self.current_block,
            slot_key_cache: self.slot_key_cache.clone(),
            tx_sender: self.tx_sender,
        }
    }
}

/// Error type for PrivacyDatabase operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivacyDatabaseError<E> {
    /// Error from the underlying database
    Inner(E),
    /// Privacy-specific error
    Privacy(String),
}

impl<E: std::fmt::Display> std::fmt::Display for PrivacyDatabaseError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inner(e) => write!(f, "database error: {}", e),
            Self::Privacy(msg) => write!(f, "privacy error: {}", msg),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for PrivacyDatabaseError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inner(e) => Some(e),
            Self::Privacy(_) => None,
        }
    }
}

impl<E> DBErrorMarker for PrivacyDatabaseError<E> {}

impl<E> From<E> for PrivacyDatabaseError<E> {
    fn from(e: E) -> Self {
        Self::Inner(e)
    }
}

impl<DB: Database> Database for PrivacyDatabase<DB>
where
    DB::Error: 'static,
{
    type Error = PrivacyDatabaseError<DB::Error>;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.basic(address).map_err(PrivacyDatabaseError::Inner)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner
            .code_by_hash(code_hash)
            .map_err(PrivacyDatabaseError::Inner)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        match self.classify(address, index) {
            SlotClassification::Public => {
                // Public slot: read from underlying database
                self.inner
                    .storage(address, index)
                    .map_err(PrivacyDatabaseError::Inner)
            }
            SlotClassification::Private { .. } => {
                // Private slot: read from private store
                Ok(self.private_store.get(address, index))
            }
        }
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner
            .block_hash(number)
            .map_err(PrivacyDatabaseError::Inner)
    }
}

impl<DB: DatabaseRef> DatabaseRef for PrivacyDatabase<DB>
where
    DB::Error: 'static,
{
    type Error = PrivacyDatabaseError<DB::Error>;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner
            .basic_ref(address)
            .map_err(PrivacyDatabaseError::Inner)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner
            .code_by_hash_ref(code_hash)
            .map_err(PrivacyDatabaseError::Inner)
    }

    fn storage_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        match self.classify(address, index) {
            SlotClassification::Public => self
                .inner
                .storage_ref(address, index)
                .map_err(PrivacyDatabaseError::Inner),
            SlotClassification::Private { .. } => Ok(self.private_store.get(address, index)),
        }
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner
            .block_hash_ref(number)
            .map_err(PrivacyDatabaseError::Inner)
    }
}

impl<DB: DatabaseCommit> DatabaseCommit for PrivacyDatabase<DB> {
    fn commit(&mut self, changes: HashMap<Address, Account>) {
        // Separate public and private storage changes
        let mut public_changes: HashMap<Address, Account> = HashMap::default();

        for (address, mut account) in changes {
            let mut private_slots: Vec<(StorageKey, EvmStorageSlot)> = Vec::new();
            let mut public_slots: HashMap<StorageKey, EvmStorageSlot> = HashMap::default();

            // Classify each storage slot
            for (slot, value) in account.storage.drain() {
                // Only process slots that actually changed
                if !Self::is_slot_changed(&value) {
                    // Unchanged slots still go to public (for cache consistency)
                    public_slots.insert(slot, value);
                    continue;
                }

                // First, try to resolve ownership from the slot key cache.
                // This records the owner in the registry so that classify() can find it.
                let _ = self.try_resolve_owner_from_cache(address, slot);

                // Now classify the slot (which may use the just-recorded owner)
                match self.classify(address, slot) {
                    SlotClassification::Public => {
                        // Check if this might be a mapping slot that we missed
                        // (e.g., inspector wasn't enabled). If so, and the contract
                        // has mapping slots configured, treat as private with fallback owner.
                        if self.is_potential_mapping_slot(address, slot) {
                            let owner = self.fallback_owner(address);
                            self.registry.record_slot_owner(address, slot, owner);
                            self.private_store.set(address, slot, value.present_value, owner);
                            private_slots.push((slot, value));
                        } else {
                            public_slots.insert(slot, value);
                        }
                    }
                    SlotClassification::Private { owner } => {
                        // Route to private store
                        self.private_store.set(
                            address,
                            slot,
                            value.present_value,
                            owner,
                        );
                        private_slots.push((slot, value));
                    }
                }
            }

            // If there are any public slots, include the account in public changes
            if !public_slots.is_empty()
                || account.status.is_touched()
                || account.info != AccountInfo::default()
            {
                account.storage = public_slots;
                public_changes.insert(address, account);
            }
        }

        // Commit public changes to the underlying database
        if !public_changes.is_empty() {
            self.inner.commit(public_changes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{OwnershipType, PrivateContractConfig, SlotConfig, SlotType};
    use revm::database::EmptyDB;
    use std::convert::Infallible;

    /// A mock database that wraps EmptyDB but also implements DatabaseCommit
    /// for testing purposes. The commit is a no-op since we're testing the
    /// privacy layer, not actual persistence.
    #[derive(Debug, Clone, Default)]
    struct MockCommitDB {
        inner: EmptyDB,
        /// Track commits for testing
        commit_count: std::cell::Cell<usize>,
    }

    impl MockCommitDB {
        fn new() -> Self {
            Self {
                inner: EmptyDB::default(),
                commit_count: std::cell::Cell::new(0),
            }
        }
    }

    impl Database for MockCommitDB {
        type Error = Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.inner.basic(address)
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            self.inner.code_by_hash(code_hash)
        }

        fn storage(
            &mut self,
            address: Address,
            index: StorageKey,
        ) -> Result<StorageValue, Self::Error> {
            self.inner.storage(address, index)
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
            self.inner.block_hash(number)
        }
    }

    impl DatabaseRef for MockCommitDB {
        type Error = Infallible;

        fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.inner.basic_ref(address)
        }

        fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            self.inner.code_by_hash_ref(code_hash)
        }

        fn storage_ref(
            &self,
            address: Address,
            index: StorageKey,
        ) -> Result<StorageValue, Self::Error> {
            self.inner.storage_ref(address, index)
        }

        fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
            self.inner.block_hash_ref(number)
        }
    }

    impl DatabaseCommit for MockCommitDB {
        fn commit(&mut self, _changes: HashMap<Address, Account>) {
            self.commit_count.set(self.commit_count.get() + 1);
        }
    }

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn setup_registry_with_private_slot(
        contract: Address,
        base_slot: U256,
    ) -> (Arc<PrivacyRegistry>, Arc<PrivateStateStore>) {
        let registry = Arc::new(PrivacyRegistry::new());
        let store = Arc::new(PrivateStateStore::new());

        let config = PrivateContractConfig {
            address: contract,
            admin: test_address(99),
            slots: vec![SlotConfig {
                base_slot,
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            }],
            registered_at: 1,
            hide_events: false,
        };

        registry.register(config).unwrap();
        (registry, store)
    }

    #[test]
    fn test_privacy_database_public_storage() {
        let contract = test_address(1);
        let registry = Arc::new(PrivacyRegistry::new());
        let store = Arc::new(PrivateStateStore::new());

        let mut db = PrivacyDatabase::new(EmptyDB::default(), registry, store);

        // Unregistered contract, slot should be public and return zero from EmptyDB
        let result = db.storage(contract, U256::from(5)).unwrap();
        assert_eq!(result, U256::ZERO);
    }

    #[test]
    fn test_privacy_database_private_storage_read() {
        let contract = test_address(1);
        let private_slot = U256::from(5);
        let (registry, store) = setup_registry_with_private_slot(contract, private_slot);

        // Pre-populate private store
        store.set(contract, private_slot, U256::from(12345), contract);

        let mut db = PrivacyDatabase::new(EmptyDB::default(), registry, store);

        // Read from private slot should return value from private store
        let result = db.storage(contract, private_slot).unwrap();
        assert_eq!(result, U256::from(12345));

        // Read from different (public) slot should return zero
        let result = db.storage(contract, U256::from(6)).unwrap();
        assert_eq!(result, U256::ZERO);
    }

    #[test]
    fn test_privacy_database_ref_storage() {
        let contract = test_address(1);
        let private_slot = U256::from(5);
        let (registry, store) = setup_registry_with_private_slot(contract, private_slot);

        store.set(contract, private_slot, U256::from(999), contract);

        let db = PrivacyDatabase::new(EmptyDB::default(), registry, store);

        // DatabaseRef should also route to private store
        let result = db.storage_ref(contract, private_slot).unwrap();
        assert_eq!(result, U256::from(999));
    }

    #[test]
    fn test_privacy_database_commit_routes_private_slots() {
        let contract = test_address(1);
        let private_slot = U256::from(5);
        let public_slot = U256::from(10);
        let (registry, store) = setup_registry_with_private_slot(contract, private_slot);

        let mut db = PrivacyDatabase::new(MockCommitDB::new(), registry, Arc::clone(&store));

        // Create account changes with both public and private slots
        let mut account = Account::new_not_existing(0);
        account.mark_touch();
        account.storage.insert(
            private_slot,
            EvmStorageSlot {
                original_value: U256::ZERO,
                present_value: U256::from(42),
                transaction_id: 0,
                is_cold: false,
            },
        );
        account.storage.insert(
            public_slot,
            EvmStorageSlot {
                original_value: U256::ZERO,
                present_value: U256::from(100),
                transaction_id: 0,
                is_cold: false,
            },
        );

        let mut changes: HashMap<Address, Account> = HashMap::default();
        changes.insert(contract, account);

        // Commit the changes
        db.commit(changes);

        // Private slot should be in private store
        assert_eq!(store.get(contract, private_slot), U256::from(42));

        // Verify owner was recorded correctly
        assert_eq!(store.get_owner(contract, private_slot), Some(contract));
    }

    #[test]
    fn test_privacy_database_unchanged_slots_not_stored() {
        let contract = test_address(1);
        let private_slot = U256::from(5);
        let (registry, store) = setup_registry_with_private_slot(contract, private_slot);

        let mut db = PrivacyDatabase::new(MockCommitDB::new(), registry, Arc::clone(&store));

        // Create account with unchanged slot (present == original)
        let mut account = Account::new_not_existing(0);
        account.mark_touch();
        account.storage.insert(
            private_slot,
            EvmStorageSlot {
                original_value: U256::from(100),
                present_value: U256::from(100), // Same as original - no change
                transaction_id: 0,
                is_cold: false,
            },
        );

        let mut changes: HashMap<Address, Account> = HashMap::default();
        changes.insert(contract, account);

        db.commit(changes);

        // Unchanged slot should NOT be written to private store
        assert_eq!(store.get(contract, private_slot), U256::ZERO);
    }

    #[test]
    fn test_privacy_database_set_block() {
        let registry = Arc::new(PrivacyRegistry::new());
        let store = Arc::new(PrivateStateStore::new());

        let mut db = PrivacyDatabase::new(EmptyDB::default(), registry, Arc::clone(&store));

        db.set_block(12345);

        assert_eq!(store.current_block(), 12345);
    }

    #[test]
    fn test_privacy_database_clone() {
        let registry = Arc::new(PrivacyRegistry::new());
        let store = Arc::new(PrivateStateStore::new());

        let db1 = PrivacyDatabase::new(EmptyDB::default(), registry, store);
        let db2 = db1.clone();

        // Both should share the same registry and store
        assert!(Arc::ptr_eq(&db1.registry, &db2.registry));
        assert!(Arc::ptr_eq(&db1.private_store, &db2.private_store));
    }

    #[test]
    fn test_privacy_database_error_display() {
        let err: PrivacyDatabaseError<std::io::Error> =
            PrivacyDatabaseError::Privacy("test error".to_string());
        assert_eq!(format!("{}", err), "privacy error: test error");

        let inner_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let err: PrivacyDatabaseError<std::io::Error> = PrivacyDatabaseError::Inner(inner_err);
        assert!(format!("{}", err).contains("database error"));
    }

    #[test]
    fn test_multiple_contracts_isolation() {
        let contract1 = test_address(1);
        let contract2 = test_address(2);
        let slot = U256::from(5);

        // Only contract1 has private slots
        let (registry, store) = setup_registry_with_private_slot(contract1, slot);

        store.set(contract1, slot, U256::from(111), contract1);

        let mut db = PrivacyDatabase::new(EmptyDB::default(), registry, store);

        // Contract1's slot is private
        assert_eq!(db.storage(contract1, slot).unwrap(), U256::from(111));

        // Contract2's slot is public (returns zero from EmptyDB)
        assert_eq!(db.storage(contract2, slot).unwrap(), U256::ZERO);
    }
}
