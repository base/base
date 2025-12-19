//! Privacy Inspector
//!
//! Intercepts SHA3 opcodes to extract mapping keys for slot ownership tracking.
//!
//! # Background
//!
//! When a mapping slot is accessed, Solidity computes the storage slot as:
//! `keccak256(abi.encode(key, base_slot))`
//!
//! This is a one-way hash, so at commit time we can't determine who owns the slot
//! without intercepting the computation. This inspector watches for SHA3 opcodes
//! with 64-byte inputs (the pattern for mapping slot computation) and records
//! the mapping between computed slots and their keys.
//!
//! # Usage
//!
//! ```ignore
//! use base_reth_privacy::inspector::{SlotKeyCache, PrivacyInspector};
//!
//! let cache = Arc::new(SlotKeyCache::new());
//! let inspector = PrivacyInspector::new(Arc::clone(&cache));
//!
//! // Use the inspector with the EVM
//! let evm = factory.create_evm_with_inspector(db, env, inspector);
//! let result = evm.transact(tx);
//!
//! // After execution, query the cache
//! if let Some((base_slot, key)) = cache.get(contract, computed_slot) {
//!     // key contains the original mapping key (e.g., owner address)
//! }
//! ```

use alloy_primitives::{Address, B256, U256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// SHA3 opcode (keccak256).
const OPCODE_SHA3: u8 = 0x20;

/// Tracks mapping key -> computed slot relationships discovered during execution.
///
/// This cache is populated by [`PrivacyInspector`] during EVM execution and can
/// be queried afterwards to determine slot ownership for mapping slots.
#[derive(Debug, Default)]
pub struct SlotKeyCache {
    /// Maps (contract, computed_slot) -> (base_slot, key)
    entries: RwLock<HashMap<(Address, U256), (U256, B256)>>,
}

impl SlotKeyCache {
    /// Creates a new empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a mapping from computed slot to its original key and base slot.
    ///
    /// # Arguments
    ///
    /// * `contract` - The contract address
    /// * `computed_slot` - The computed storage slot (result of keccak256)
    /// * `base_slot` - The base slot from the mapping declaration
    /// * `key` - The mapping key (32 bytes, may contain an address)
    pub fn insert(&self, contract: Address, computed_slot: U256, base_slot: U256, key: B256) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert((contract, computed_slot), (base_slot, key));
        }
    }

    /// Looks up the original key for a computed slot.
    ///
    /// # Returns
    ///
    /// - `Some((base_slot, key))` if this slot was observed being computed
    /// - `None` if the slot wasn't computed during execution (or is not a mapping)
    pub fn get(&self, contract: Address, slot: U256) -> Option<(U256, B256)> {
        self.entries
            .read()
            .ok()
            .and_then(|entries| entries.get(&(contract, slot)).cloned())
    }

    /// Clears all cached entries.
    ///
    /// Should be called between transactions to avoid stale data.
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }

    /// Returns the number of entries in the cache.
    pub fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    /// Returns true if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Inspector that intercepts SHA3 opcodes to extract mapping keys.
///
/// When a SHA3 operation with a 64-byte input is detected (the signature of
/// mapping slot computation), this inspector records the relationship between
/// the computed slot and the original key + base_slot.
#[derive(Debug)]
pub struct PrivacyInspector {
    /// Current contract being executed.
    current_contract: Option<Address>,
    /// Cache for discovered slot keys.
    cache: Arc<SlotKeyCache>,
}

impl PrivacyInspector {
    /// Creates a new privacy inspector with the given cache.
    pub fn new(cache: Arc<SlotKeyCache>) -> Self {
        Self {
            current_contract: None,
            cache,
        }
    }

    /// Returns a reference to the slot key cache.
    pub fn cache(&self) -> &Arc<SlotKeyCache> {
        &self.cache
    }

    /// Sets the current contract address.
    ///
    /// This should be called when a CALL is made to track which contract
    /// is computing the slot.
    pub fn set_current_contract(&mut self, address: Address) {
        self.current_contract = Some(address);
    }

    /// Clears the current contract address.
    pub fn clear_current_contract(&mut self) {
        self.current_contract = None;
    }
}

// Note: The full Inspector implementation requires complex trait bounds that
// vary by revm version. For now, we provide the core data structures and logic.
// The actual Inspector trait implementation will be added when integrating with
// the processor, as it requires access to the specific types used there.
//
// The key logic that would go in the `step` method:
//
// 1. Check if opcode is SHA3 (0x20)
// 2. Read offset and size from stack (stack[0] = offset, stack[1] = size)
// 3. If size == 64, this is likely a mapping slot computation
// 4. Read 64 bytes from memory at offset
// 5. Parse: first 32 bytes = key, second 32 bytes = base_slot
// 6. Compute the result: keccak256(memory[offset..offset+64])
// 7. Store in cache: (contract, computed_slot) -> (base_slot, key)

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = SlotKeyCache::new();
        let contract = test_address(1);
        let computed_slot = U256::from(12345);
        let base_slot = U256::from(0);
        let key = B256::from([0xAB; 32]);

        cache.insert(contract, computed_slot, base_slot, key);

        let result = cache.get(contract, computed_slot);
        assert_eq!(result, Some((base_slot, key)));
    }

    #[test]
    fn test_cache_get_nonexistent() {
        let cache = SlotKeyCache::new();
        let contract = test_address(1);
        let slot = U256::from(12345);

        assert_eq!(cache.get(contract, slot), None);
    }

    #[test]
    fn test_cache_clear() {
        let cache = SlotKeyCache::new();
        let contract = test_address(1);
        let slot = U256::from(12345);
        let base_slot = U256::ZERO;
        let key = B256::ZERO;

        cache.insert(contract, slot, base_slot, key);
        assert!(!cache.is_empty());

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.get(contract, slot), None);
    }

    #[test]
    fn test_cache_multiple_contracts() {
        let cache = SlotKeyCache::new();
        let contract1 = test_address(1);
        let contract2 = test_address(2);
        let slot = U256::from(42);
        let base_slot = U256::ZERO;
        let key1 = B256::from([0x11; 32]);
        let key2 = B256::from([0x22; 32]);

        cache.insert(contract1, slot, base_slot, key1);
        cache.insert(contract2, slot, base_slot, key2);

        assert_eq!(cache.get(contract1, slot), Some((base_slot, key1)));
        assert_eq!(cache.get(contract2, slot), Some((base_slot, key2)));
    }

    #[test]
    fn test_inspector_creation() {
        let cache = Arc::new(SlotKeyCache::new());
        let inspector = PrivacyInspector::new(Arc::clone(&cache));

        assert!(inspector.current_contract.is_none());
        assert!(Arc::ptr_eq(&inspector.cache, &cache));
    }

    #[test]
    fn test_inspector_set_contract() {
        let cache = Arc::new(SlotKeyCache::new());
        let mut inspector = PrivacyInspector::new(cache);
        let contract = test_address(1);

        inspector.set_current_contract(contract);
        assert_eq!(inspector.current_contract, Some(contract));

        inspector.clear_current_contract();
        assert!(inspector.current_contract.is_none());
    }
}
