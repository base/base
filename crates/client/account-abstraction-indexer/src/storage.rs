//! UserOperation storage backend
//!
//! Provides storage for indexed UserOperations, supporting lookups by hash.
//! Currently uses an in-memory store; can be extended to use persistent storage.
//!
//! This storage uses minimal `IndexedUserOperationRef` entries (~80 bytes each)
//! instead of full event data (~300 bytes) to reduce memory usage.

use alloy_primitives::B256;
use parking_lot::RwLock;
use std::collections::HashMap;

use crate::event::IndexedUserOperationRef;

/// Storage backend for indexed UserOperations
/// 
/// This implementation uses an in-memory HashMap with minimal storage per entry.
/// Only the data needed to locate the full event (tx hash + log index) is stored.
/// Full event data can be re-hydrated from the transaction receipt when needed.
///
/// For production use with persistence requirements, this can be extended to 
/// use RocksDB or SQLite.
#[derive(Debug, Default)]
pub struct UserOperationStorage {
    /// Map from userOpHash -> minimal reference (tx hash + log index)
    operations: RwLock<HashMap<B256, IndexedUserOperationRef>>,
    /// Map from block_number -> list of userOpHashes (for reorg handling)
    by_block: RwLock<HashMap<u64, Vec<B256>>>,
}

impl UserOperationStorage {
    /// Create a new empty storage
    pub fn new() -> Self {
        Self::default()
    }

    /// Index a new UserOperation reference
    pub fn insert(&self, op_ref: IndexedUserOperationRef) {
        let hash = op_ref.user_op_hash;
        let block_number = op_ref.block_number;

        // Insert into main index
        self.operations.write().insert(hash, op_ref);

        // Track by block for reorg handling
        self.by_block
            .write()
            .entry(block_number)
            .or_default()
            .push(hash);

        metrics::counter!("aa_indexer_operations_indexed").increment(1);
    }

    /// Get a UserOperation reference by its hash
    ///
    /// Returns the minimal reference containing tx_hash and log_index.
    /// Use these to fetch the receipt and re-hydrate full event data.
    pub fn get(&self, user_op_hash: &B256) -> Option<IndexedUserOperationRef> {
        self.operations.read().get(user_op_hash).copied()
    }

    /// Check if a UserOperation exists
    pub fn contains(&self, user_op_hash: &B256) -> bool {
        self.operations.read().contains_key(user_op_hash)
    }

    /// Remove all UserOperations from a specific block (for reorg handling)
    /// 
    /// Returns the number of operations removed
    pub fn remove_block(&self, block_number: u64) -> usize {
        // Acquire both locks to ensure atomicity
        let mut by_block = self.by_block.write();
        let mut operations = self.operations.write();
        
        self.remove_block_internal(&mut by_block, &mut operations, block_number)
    }

    /// Remove all UserOperations from blocks >= the given block number (for reorg handling)
    /// 
    /// Returns the number of operations removed
    /// 
    /// This method acquires both locks atomically to prevent race conditions
    /// where another thread could insert between collecting blocks and removing them.
    pub fn remove_from_block(&self, from_block: u64) -> usize {
        // Acquire both locks upfront to ensure atomicity
        let mut by_block = self.by_block.write();
        let mut operations = self.operations.write();
        
        // Collect blocks to remove while holding the lock
        let blocks_to_remove: Vec<u64> = by_block
            .keys()
            .filter(|&&b| b >= from_block)
            .copied()
            .collect();

        let mut total_removed = 0;
        for block_number in blocks_to_remove {
            total_removed += self.remove_block_internal(&mut by_block, &mut operations, block_number);
        }
        total_removed
    }
    
    /// Internal helper to remove a block's operations while holding locks
    /// 
    /// Caller must hold write locks on both `by_block` and `operations`.
    fn remove_block_internal(
        &self,
        by_block: &mut HashMap<u64, Vec<B256>>,
        operations: &mut HashMap<B256, IndexedUserOperationRef>,
        block_number: u64,
    ) -> usize {
        let hashes = by_block.remove(&block_number);

        if let Some(hashes) = hashes {
            let count = hashes.len();
            for hash in hashes {
                operations.remove(&hash);
            }
            metrics::counter!("aa_indexer_operations_removed").increment(count as u64);
            count
        } else {
            0
        }
    }

    /// Get the total number of indexed operations
    pub fn len(&self) -> usize {
        self.operations.read().len()
    }

    /// Check if storage is empty
    pub fn is_empty(&self) -> bool {
        self.operations.read().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_ref(hash: B256, block_number: u64) -> IndexedUserOperationRef {
        IndexedUserOperationRef {
            user_op_hash: hash,
            transaction_hash: B256::repeat_byte(0xaa),
            log_index: 0,
            block_number,
        }
    }

    #[test]
    fn test_insert_and_get() {
        let storage = UserOperationStorage::new();
        let hash = B256::repeat_byte(0x01);
        let op_ref = create_test_ref(hash, 100);

        storage.insert(op_ref);

        let retrieved = storage.get(&hash).unwrap();
        assert_eq!(retrieved.user_op_hash, hash);
        assert_eq!(retrieved.block_number, 100);
    }

    #[test]
    fn test_remove_block() {
        let storage = UserOperationStorage::new();
        
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);

        storage.insert(create_test_ref(hash1, 100));
        storage.insert(create_test_ref(hash2, 100));
        storage.insert(create_test_ref(hash3, 101));

        assert_eq!(storage.len(), 3);

        let removed = storage.remove_block(100);
        assert_eq!(removed, 2);
        assert_eq!(storage.len(), 1);
        assert!(storage.get(&hash1).is_none());
        assert!(storage.get(&hash2).is_none());
        assert!(storage.get(&hash3).is_some());
    }

    #[test]
    fn test_remove_from_block() {
        let storage = UserOperationStorage::new();

        storage.insert(create_test_ref(B256::repeat_byte(0x01), 100));
        storage.insert(create_test_ref(B256::repeat_byte(0x02), 101));
        storage.insert(create_test_ref(B256::repeat_byte(0x03), 102));

        let removed = storage.remove_from_block(101);
        assert_eq!(removed, 2);
        assert_eq!(storage.len(), 1);
    }
}

