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

use alloy_primitives::{keccak256, Address, B256, U256};
use revm::interpreter::{CallInputs, CallOutcome, Interpreter, InterpreterTypes};
use revm::Inspector;
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

    /// Process a SHA3 operation to extract mapping key information.
    ///
    /// This is called from the `step` method when we detect a SHA3 opcode.
    /// If the input is 64 bytes (mapping slot pattern), we record the relationship.
    fn process_sha3<INTR: InterpreterTypes>(&mut self, interp: &Interpreter<INTR>) {
        use revm::interpreter::interpreter_types::{MemoryTr, StackTr};

        let Some(contract) = self.current_contract else {
            return;
        };

        // SHA3 takes (offset, size) from stack
        // Stack: [..., size, offset] - offset is at top (index 0), size is at index 1
        let stack_data = interp.stack.data();
        if stack_data.len() < 2 {
            return;
        }

        // Get offset and size from stack (top of stack is last element)
        let stack_len = stack_data.len();
        let offset = stack_data[stack_len - 1]; // top of stack
        let size = stack_data[stack_len - 2]; // second from top

        // We're looking for mapping slot computation: keccak256(abi.encode(key, base_slot))
        // This is 64 bytes: 32-byte key + 32-byte base_slot
        if size != U256::from(64) {
            return;
        }

        let Ok(offset_usize) = TryInto::<usize>::try_into(offset) else {
            return;
        };

        // Check memory bounds
        if interp.memory.size() < offset_usize + 64 {
            return;
        }

        // Read the 64 bytes from memory
        let data_slice = interp.memory.slice(offset_usize..offset_usize + 64);
        let mut data = [0u8; 64];
        data.copy_from_slice(&data_slice);
        drop(data_slice);

        // Parse: first 32 bytes = key, second 32 bytes = base_slot
        let key = B256::from_slice(&data[0..32]);
        let base_slot = U256::from_be_bytes::<32>(data[32..64].try_into().unwrap());

        // Compute what the result will be (same as EVM will compute)
        let computed_hash = keccak256(&data);
        let computed_slot = U256::from_be_bytes(computed_hash.0);

        // Store in cache for later lookup
        self.cache.insert(contract, computed_slot, base_slot, key);
    }
}

/// Implementation of the revm Inspector trait for PrivacyInspector.
///
/// This implementation intercepts:
/// - `step`: To detect SHA3 opcodes and extract mapping keys
/// - `call`: To track the current contract address
impl<CTX, INTR> Inspector<CTX, INTR> for PrivacyInspector
where
    INTR: InterpreterTypes,
    INTR::Bytecode: revm::interpreter::interpreter_types::Jumps,
    INTR::Stack: revm::interpreter::interpreter_types::StackTr,
    INTR::Memory: revm::interpreter::interpreter_types::MemoryTr,
{
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        use revm::interpreter::interpreter_types::Jumps;

        // Check if this is SHA3 opcode (0x20)
        let opcode = interp.bytecode.opcode();
        if opcode == OPCODE_SHA3 {
            self.process_sha3(interp);
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        // Track current contract being called
        self.current_contract = Some(inputs.target_address);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        // Optionally clear contract on call end, but we typically want to keep it
        // for the duration of nested calls. The contract will be updated on next call.
    }
}

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

    #[test]
    fn test_mapping_slot_computation() {
        // This test verifies that our slot computation matches Solidity's
        // mapping slot formula: keccak256(abi.encode(key, base_slot))

        // Example: mapping(address => uint256) balances at slot 0
        // For address 0x1111...1111 accessing balances[address]:
        // computed_slot = keccak256(abi.encode(address, 0))

        let owner = Address::new([0x11; 20]);
        let base_slot = U256::ZERO;

        // Build the 64-byte input as Solidity would
        // First 32 bytes: key (address left-padded to 32 bytes)
        // Second 32 bytes: base_slot
        let mut data = [0u8; 64];
        data[12..32].copy_from_slice(owner.as_slice()); // address is 20 bytes, left-pad with 12 zeros
        data[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());

        // Compute the slot
        let computed_hash = keccak256(&data);
        let computed_slot = U256::from_be_bytes(computed_hash.0);

        // Store in cache as the inspector would
        let cache = SlotKeyCache::new();
        let contract = test_address(1);
        let key = B256::from_slice(&data[0..32]);

        cache.insert(contract, computed_slot, base_slot, key);

        // Verify we can look it up
        let result = cache.get(contract, computed_slot);
        assert!(result.is_some());

        let (retrieved_base, retrieved_key) = result.unwrap();
        assert_eq!(retrieved_base, base_slot);
        assert_eq!(retrieved_key, key);

        // Verify the key contains the owner address (last 20 bytes)
        let key_address = Address::from_slice(&retrieved_key[12..32]);
        assert_eq!(key_address, owner);
    }

    #[test]
    fn test_nested_mapping_slot_computation() {
        // Test nested mapping: mapping(address => mapping(uint256 => uint256))
        // E.g., allowances[owner][spender]

        let owner = Address::new([0x11; 20]);
        let spender = Address::new([0x22; 20]);
        let base_slot = U256::from(1);

        // First level: keccak256(abi.encode(owner, base_slot))
        let mut data1 = [0u8; 64];
        data1[12..32].copy_from_slice(owner.as_slice());
        data1[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
        let slot1_hash = keccak256(&data1);
        let slot1 = U256::from_be_bytes(slot1_hash.0);

        // Second level: keccak256(abi.encode(spender, slot1))
        let mut data2 = [0u8; 64];
        data2[12..32].copy_from_slice(spender.as_slice());
        data2[32..64].copy_from_slice(&slot1.to_be_bytes::<32>());
        let slot2_hash = keccak256(&data2);
        let slot2 = U256::from_be_bytes(slot2_hash.0);

        // Cache would capture both computations
        let cache = SlotKeyCache::new();
        let contract = test_address(1);

        cache.insert(contract, slot1, base_slot, B256::from_slice(&data1[0..32]));
        cache.insert(contract, slot2, slot1, B256::from_slice(&data2[0..32]));

        // Can look up the second level
        let result = cache.get(contract, slot2);
        assert!(result.is_some());

        let (retrieved_base, retrieved_key) = result.unwrap();
        // The base for slot2 is slot1 (the result of first hash)
        assert_eq!(retrieved_base, slot1);
        // The key contains the spender address
        let key_address = Address::from_slice(&retrieved_key[12..32]);
        assert_eq!(key_address, spender);
    }
}
