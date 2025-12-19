//! Integration tests for the privacy layer.
//!
//! These tests verify the complete privacy flow:
//! 1. Contract registration via Registry
//! 2. Slot authorization via PrivateStateStore
//! 3. Private state storage and retrieval
//! 4. RPC filtering for unauthorized callers
//!
//! Note: These tests run the privacy layer components in isolation (without a full node).
//! For full E2E tests with flashblocks, see `crates/rpc/tests/`.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_reth_privacy::{
    PrivacyDatabase, PrivacyRegistry, PrivateStateStore,
    inspector::SlotKeyCache,
    registry::{PrivateContractConfig, OwnershipType, SlotConfig, SlotType},
    rpc::PrivacyRpcFilter,
    store::{AuthEntry, READ},
};
use revm::database_interface::{Database, DatabaseRef};

// Helper to create a test address
fn test_address(n: u8) -> Address {
    Address::new([n; 20])
}

// Helper to compute a mapping slot like Solidity does
// keccak256(abi.encode(key, base_slot))
fn compute_mapping_slot(key: Address, base_slot: U256) -> U256 {
    let mut data = [0u8; 64];
    data[12..32].copy_from_slice(key.as_slice());
    data[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
    let hash = keccak256(&data);
    U256::from_be_bytes(hash.0)
}

// Helper to extract the key from a computed slot using the cache
fn key_from_data(data: &[u8; 64]) -> B256 {
    B256::from_slice(&data[0..32])
}

/// Mock database for testing - stores account/storage in memory
#[derive(Default, Clone)]
struct MockDatabase {
    storage: std::collections::HashMap<(Address, U256), U256>,
}

impl Database for MockDatabase {
    type Error = std::convert::Infallible;

    fn basic(
        &mut self,
        _address: Address,
    ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        Ok(Some(revm::state::AccountInfo::default()))
    }

    fn code_by_hash(
        &mut self,
        _code_hash: B256,
    ) -> Result<revm::bytecode::Bytecode, Self::Error> {
        Ok(revm::bytecode::Bytecode::default())
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.storage.get(&(address, index)).copied().unwrap_or(U256::ZERO))
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        Ok(B256::ZERO)
    }
}

impl DatabaseRef for MockDatabase {
    type Error = std::convert::Infallible;

    fn basic_ref(
        &self,
        _address: Address,
    ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        Ok(Some(revm::state::AccountInfo::default()))
    }

    fn code_by_hash_ref(
        &self,
        _code_hash: B256,
    ) -> Result<revm::bytecode::Bytecode, Self::Error> {
        Ok(revm::bytecode::Bytecode::default())
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.storage.get(&(address, index)).copied().unwrap_or(U256::ZERO))
    }

    fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
        Ok(B256::ZERO)
    }
}

// Helper to create a private contract config
fn create_config(contract: Address, admin: Address, slots: Vec<SlotConfig>, hide_events: bool) -> PrivateContractConfig {
    PrivateContractConfig {
        address: contract,
        admin,
        slots,
        registered_at: 0,
        hide_events,
    }
}

// ============================================================================
// Registry Tests
// ============================================================================

#[test]
fn test_register_contract_with_mapping_slot() {
    let registry = PrivacyRegistry::new();
    let contract = test_address(1);
    let admin = test_address(2);

    // Configure a mapping slot at base slot 1 with MappingKey ownership
    let config = create_config(
        contract,
        admin,
        vec![SlotConfig {
            base_slot: U256::from(1),
            slot_type: SlotType::Mapping,
            ownership: OwnershipType::MappingKey,
        }],
        false,
    );

    registry.register(config).unwrap();

    assert!(registry.is_registered(&contract));
    assert!(!registry.is_registered(&test_address(99))); // Not registered
}

#[test]
fn test_slot_owner_tracking() {
    let registry = PrivacyRegistry::new();
    let contract = test_address(1);
    let admin = test_address(2);
    let user = test_address(10);

    // Register contract with mapping slot
    let config = create_config(
        contract,
        admin,
        vec![SlotConfig {
            base_slot: U256::from(1),
            slot_type: SlotType::Mapping,
            ownership: OwnershipType::MappingKey,
        }],
        false,
    );
    registry.register(config).unwrap();

    // Compute the slot for user's balance
    let computed_slot = compute_mapping_slot(user, U256::from(1));

    // Record the owner
    registry.record_slot_owner(contract, computed_slot, user);

    // Verify we can look up the owner
    let owner = registry.get_slot_owner(contract, computed_slot);
    assert_eq!(owner, Some(user));
}

// ============================================================================
// Private Store Tests
// ============================================================================

#[test]
fn test_private_store_set_and_get() {
    let store = PrivateStateStore::new();
    let contract = test_address(1);
    let slot = U256::from(42);
    let value = U256::from(12345);
    let owner = test_address(10);

    store.set(contract, slot, value, owner);

    // Get returns the value
    let retrieved = store.get(contract, slot);
    assert_eq!(retrieved, value);
}

#[test]
fn test_private_store_authorization() {
    let store = PrivateStateStore::new();
    let contract = test_address(1);
    let slot = U256::from(42);
    let owner = test_address(10);
    let delegate = test_address(20);
    let value = U256::from(12345);

    // First set a value with owner
    store.set(contract, slot, value, owner);

    // Owner is always authorized (has owner entry)
    assert!(store.is_authorized(contract, slot, owner, READ));

    // Delegate is not authorized initially
    assert!(!store.is_authorized(contract, slot, delegate, READ));

    // Grant read access to delegate
    store.authorize(contract, slot, delegate, AuthEntry::new(READ, 0, 0));

    // Now delegate is authorized
    assert!(store.is_authorized(contract, slot, delegate, READ));
}

// ============================================================================
// Privacy Database Integration Tests
// ============================================================================

#[test]
fn test_privacy_database_public_slot_passthrough() {
    let mock_db = MockDatabase::default();
    let registry = Arc::new(PrivacyRegistry::new());
    let store = Arc::new(PrivateStateStore::new());

    let mut privacy_db = PrivacyDatabase::new(mock_db.clone(), registry, store);

    let contract = test_address(1);
    let slot = U256::from(0);

    // Contract is not registered, so all slots are public (passthrough)
    let value = privacy_db.storage(contract, slot).unwrap();
    assert_eq!(value, U256::ZERO); // Mock returns ZERO for unset slots
}

#[test]
fn test_privacy_database_with_slot_key_cache() {
    let mut mock_db = MockDatabase::default();
    let registry = Arc::new(PrivacyRegistry::new());
    let store = Arc::new(PrivateStateStore::new());

    // Set up a value in the mock database
    let contract = test_address(1);
    let user = test_address(10);
    let base_slot = U256::from(1);
    let computed_slot = compute_mapping_slot(user, base_slot);
    let value = U256::from(1000);

    mock_db.storage.insert((contract, computed_slot), value);

    // Register the contract with a mapping slot
    let admin = test_address(2);
    let config = create_config(
        contract,
        admin,
        vec![SlotConfig {
            base_slot,
            slot_type: SlotType::Mapping,
            ownership: OwnershipType::MappingKey,
        }],
        false,
    );
    registry.register(config).unwrap();

    // Create the slot key cache and populate it (simulating inspector behavior)
    let cache = Arc::new(SlotKeyCache::new());
    let mut data = [0u8; 64];
    data[12..32].copy_from_slice(user.as_slice());
    data[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
    let key = key_from_data(&data);
    cache.insert(contract, computed_slot, base_slot, key);

    // Create privacy database with the cache
    let mut privacy_db = PrivacyDatabase::new(mock_db, Arc::clone(&registry), Arc::clone(&store));
    privacy_db.set_slot_key_cache(cache);
    privacy_db.set_tx_sender(user); // User is the transaction sender

    // The slot should be classified as private (mapping slot with registered contract)
    // When we read, it should work because we haven't committed yet (reads go to inner db)
    let read_value = privacy_db.storage(contract, computed_slot).unwrap();
    assert_eq!(read_value, value);
}

// ============================================================================
// RPC Privacy Filter Tests
// ============================================================================

#[test]
fn test_rpc_filter_allows_public_slots() {
    let registry = Arc::new(PrivacyRegistry::new());
    let store = Arc::new(PrivateStateStore::new());
    let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

    let contract = test_address(1);
    let slot = U256::from(0);
    let caller = Some(test_address(10));
    let value = B256::from(U256::from(12345));

    // Contract not registered - all slots are public
    let filtered = filter.filter_storage(contract, slot, value, caller);
    assert_eq!(filtered, value);
}

#[test]
fn test_rpc_filter_hides_unauthorized_private_slots() {
    let registry = Arc::new(PrivacyRegistry::new());
    let store = Arc::new(PrivateStateStore::new());

    // Register a contract with a simple private slot
    let contract = test_address(1);
    let admin = test_address(2);
    let owner = test_address(10);
    let unauthorized = test_address(99);

    let slot = U256::from(5);
    let value = B256::from(U256::from(12345));

    let config = create_config(
        contract,
        admin,
        vec![SlotConfig {
            base_slot: slot,
            slot_type: SlotType::Simple,
            ownership: OwnershipType::FixedOwner(owner),
        }],
        false,
    );
    registry.register(config).unwrap();

    // Store the private value
    store.set(contract, slot, U256::from(12345), owner);

    let filter = PrivacyRpcFilter::new(Some(Arc::clone(&registry)), Some(Arc::clone(&store)));

    // Owner can see the value
    let filtered_for_owner = filter.filter_storage(contract, slot, value, Some(owner));
    assert_eq!(filtered_for_owner, value);

    // Admin can see the value (admin check depends on how classify_slot works)
    // Note: The admin check is in is_caller_authorized but may not be implemented
    // For now, test that owner works

    // Unauthorized user sees zero
    let filtered_for_unauthorized = filter.filter_storage(contract, slot, value, Some(unauthorized));
    assert_eq!(filtered_for_unauthorized, B256::ZERO);
}

#[test]
fn test_rpc_filter_with_delegated_access() {
    let registry = Arc::new(PrivacyRegistry::new());
    let store = Arc::new(PrivateStateStore::new());

    // Register a contract with a private slot
    let contract = test_address(1);
    let admin = test_address(2);
    let owner = test_address(10);
    let delegate = test_address(20);

    let slot = U256::from(5);
    let value = B256::from(U256::from(12345));

    let config = create_config(
        contract,
        admin,
        vec![SlotConfig {
            base_slot: slot,
            slot_type: SlotType::Simple,
            ownership: OwnershipType::FixedOwner(owner),
        }],
        false,
    );
    registry.register(config).unwrap();

    // Store the private value
    store.set(contract, slot, U256::from(12345), owner);

    let filter = PrivacyRpcFilter::new(Some(Arc::clone(&registry)), Some(Arc::clone(&store)));

    // Delegate cannot see the value initially
    let filtered = filter.filter_storage(contract, slot, value, Some(delegate));
    assert_eq!(filtered, B256::ZERO);

    // Grant read access to delegate
    store.authorize(contract, slot, delegate, AuthEntry::new(READ, 0, 0));

    // Now delegate can see the value
    let filtered = filter.filter_storage(contract, slot, value, Some(delegate));
    assert_eq!(filtered, value);
}

// ============================================================================
// Inspector Slot Key Cache Tests
// ============================================================================

#[test]
fn test_slot_key_cache_mapping_lookup() {
    let cache = SlotKeyCache::new();

    let contract = test_address(1);
    let user = test_address(10);
    let base_slot = U256::from(1);

    // Compute the slot as the inspector would
    let computed_slot = compute_mapping_slot(user, base_slot);

    // Build the key (first 32 bytes of input to keccak)
    let mut data = [0u8; 64];
    data[12..32].copy_from_slice(user.as_slice());
    data[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
    let key = B256::from_slice(&data[0..32]);

    // Insert into cache
    cache.insert(contract, computed_slot, base_slot, key);

    // Look it up
    let result = cache.get(contract, computed_slot);
    assert!(result.is_some());

    let (retrieved_base, retrieved_key) = result.unwrap();
    assert_eq!(retrieved_base, base_slot);
    assert_eq!(retrieved_key, key);

    // Extract the address from the key
    let extracted_address = Address::from_slice(&retrieved_key[12..32]);
    assert_eq!(extracted_address, user);
}

#[test]
fn test_slot_key_cache_nested_mapping() {
    let cache = SlotKeyCache::new();

    let contract = test_address(1);
    let owner = test_address(10);
    let spender = test_address(20);
    let base_slot = U256::from(2); // allowances mapping at slot 2

    // First level: keccak256(abi.encode(owner, base_slot))
    let slot1 = compute_mapping_slot(owner, base_slot);

    // Second level: keccak256(abi.encode(spender, slot1))
    let slot2 = compute_mapping_slot(spender, slot1);

    // Build keys
    let mut data1 = [0u8; 64];
    data1[12..32].copy_from_slice(owner.as_slice());
    data1[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
    let key1 = B256::from_slice(&data1[0..32]);

    let mut data2 = [0u8; 64];
    data2[12..32].copy_from_slice(spender.as_slice());
    data2[32..64].copy_from_slice(&slot1.to_be_bytes::<32>());
    let key2 = B256::from_slice(&data2[0..32]);

    // Insert both levels
    cache.insert(contract, slot1, base_slot, key1);
    cache.insert(contract, slot2, slot1, key2);

    // Look up second level
    let result = cache.get(contract, slot2);
    assert!(result.is_some());

    let (retrieved_base, retrieved_key) = result.unwrap();
    // The base is the first level slot
    assert_eq!(retrieved_base, slot1);
    // The key contains the spender
    let extracted_spender = Address::from_slice(&retrieved_key[12..32]);
    assert_eq!(extracted_spender, spender);

    // Can trace back to get the owner from slot1
    let result1 = cache.get(contract, slot1);
    assert!(result1.is_some());
    let (_, key1_retrieved) = result1.unwrap();
    let extracted_owner = Address::from_slice(&key1_retrieved[12..32]);
    assert_eq!(extracted_owner, owner);
}

// ============================================================================
// Event Filtering Tests
// ============================================================================

// Helper to create a test log compatible with RPC types
fn create_test_log(address: Address, data: U256) -> alloy_rpc_types_eth::Log {
    alloy_rpc_types_eth::Log {
        inner: alloy_primitives::Log {
            address,
            data: alloy_primitives::LogData::new_unchecked(
                vec![],
                Bytes::from(data.to_be_bytes::<32>().to_vec()),
            ),
        },
        block_hash: Some(B256::ZERO),
        block_number: Some(1),
        block_timestamp: None,
        transaction_hash: Some(B256::ZERO),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
}

#[test]
fn test_event_filtering_hide_events() {
    let registry = Arc::new(PrivacyRegistry::new());
    let store = Arc::new(PrivateStateStore::new());

    // Register a contract with hide_events = true
    let contract = test_address(1);
    let admin = test_address(2);

    let config = create_config(contract, admin, vec![], true);
    registry.register(config).unwrap();

    let filter = PrivacyRpcFilter::new(Some(Arc::clone(&registry)), Some(store));

    // Create a test log from the private contract
    let log = create_test_log(contract, U256::from(100));

    // Admin can see the log
    let filtered = filter.filter_logs(vec![log.clone()], Some(admin));
    assert_eq!(filtered.len(), 1);

    // Random user cannot see the log
    let unauthorized = test_address(99);
    let filtered = filter.filter_logs(vec![log.clone()], Some(unauthorized));
    assert_eq!(filtered.len(), 0);
}

#[test]
fn test_event_filtering_public_contract() {
    let registry = Arc::new(PrivacyRegistry::new());
    let store = Arc::new(PrivateStateStore::new());

    // Register a contract with hide_events = false
    let contract = test_address(1);
    let admin = test_address(2);

    let config = create_config(contract, admin, vec![], false);
    registry.register(config).unwrap();

    let filter = PrivacyRpcFilter::new(Some(Arc::clone(&registry)), Some(store));

    // Create a test log
    let log = create_test_log(contract, U256::from(100));

    // Anyone can see the log when hide_events is false
    let unauthorized = test_address(99);
    let filtered = filter.filter_logs(vec![log], Some(unauthorized));
    assert_eq!(filtered.len(), 1);
}
