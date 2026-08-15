//! Common test suite for [`BaseProofsStore`] implementations.

mod branch_cursors;
mod hashed_cursors;
mod history;
mod proof_window;
mod trie_updates;

use std::sync::Arc;

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorageError, BaseProofsStorageResult, BaseProofsStore,
    BlockStateDiff, InMemoryProofsStorage,
    api::{InitialStateAnchor, WriteCounts},
    db::{MdbxProofsStorage, RocksdbProofsStorage},
};
use reth_primitives_traits::Account;
use reth_trie::{
    BranchNodeCompact, HashedPostState, HashedPostStateSorted, HashedStorage, Nibbles, TrieMask,
    hashed_cursor::HashedCursor,
    trie_cursor::TrieCursor,
    updates::{StorageTrieUpdates, TrieUpdates, TrieUpdatesSorted},
};
use tempfile::TempDir;

/// Helper to create a simple test branch node
fn create_test_branch() -> BranchNodeCompact {
    let mut state_mask = TrieMask::default();
    state_mask.set_bit(0);
    state_mask.set_bit(1);

    BranchNodeCompact {
        state_mask,
        tree_mask: TrieMask::default(),
        hash_mask: TrieMask::default(),
        hashes: Arc::new(vec![]),
        root_hash: None,
    }
}

/// Helper to create a variant test branch node for comparison tests
fn create_test_branch_variant() -> BranchNodeCompact {
    let mut state_mask = TrieMask::default();
    state_mask.set_bit(5);
    state_mask.set_bit(6);

    BranchNodeCompact {
        state_mask,
        tree_mask: TrieMask::default(),
        hash_mask: TrieMask::default(),
        hashes: Arc::new(vec![]),
        root_hash: None,
    }
}

/// Helper to create nibbles from a vector of u8 values
fn nibbles_from(vec: Vec<u8>) -> Nibbles {
    Nibbles::from_nibbles_unchecked(vec)
}

/// Helper to create a test account
fn create_test_account() -> Account {
    Account {
        nonce: 42,
        balance: U256::from(1000000),
        bytecode_hash: Some(B256::repeat_byte(0xBB)),
    }
}

/// Helper to create a test account with custom values
fn create_test_account_with_values(nonce: u64, balance: u64, code_hash_byte: u8) -> Account {
    Account {
        nonce,
        balance: U256::from(balance),
        bytecode_hash: Some(B256::repeat_byte(code_hash_byte)),
    }
}

fn create_mdbx_proofs_storage() -> MdbxProofsStorage {
    let path = TempDir::new().unwrap();
    MdbxProofsStorage::new(path.path()).unwrap()
}

#[derive(Debug)]
struct TestRocksdbProofsStorage {
    storage: RocksdbProofsStorage,
    _dir: TempDir,
}

fn create_rocksdb_proofs_storage() -> TestRocksdbProofsStorage {
    let dir = TempDir::new().unwrap();
    let storage = RocksdbProofsStorage::new(dir.path()).unwrap();
    TestRocksdbProofsStorage { storage, _dir: dir }
}

impl BaseProofsStore for TestRocksdbProofsStorage {
    type StorageTrieCursor<'tx>
        = <RocksdbProofsStorage as BaseProofsStore>::StorageTrieCursor<'tx>
    where
        Self: 'tx;
    type AccountTrieCursor<'tx>
        = <RocksdbProofsStorage as BaseProofsStore>::AccountTrieCursor<'tx>
    where
        Self: 'tx;
    type StorageCursor<'tx>
        = <RocksdbProofsStorage as BaseProofsStore>::StorageCursor<'tx>
    where
        Self: 'tx;
    type AccountHashedCursor<'tx>
        = <RocksdbProofsStorage as BaseProofsStore>::AccountHashedCursor<'tx>
    where
        Self: 'tx;
    type Tx<'tx>
        = <RocksdbProofsStorage as BaseProofsStore>::Tx<'tx>
    where
        Self: 'tx;

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.storage.get_earliest_block_number()
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.storage.get_latest_block_number()
    }

    fn storage_trie_cursor<'tx>(
        &'tx self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        self.storage.storage_trie_cursor(hashed_address, max_block_number)
    }

    fn account_trie_cursor<'tx>(
        &'tx self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        self.storage.account_trie_cursor(max_block_number)
    }

    fn storage_hashed_cursor<'tx>(
        &'tx self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>> {
        self.storage.storage_hashed_cursor(hashed_address, max_block_number)
    }

    fn account_hashed_cursor<'tx>(
        &'tx self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        self.storage.account_hashed_cursor(max_block_number)
    }

    fn ro_tx<'tx>(&'tx self) -> BaseProofsStorageResult<Self::Tx<'tx>> {
        self.storage.ro_tx()
    }

    fn storage_trie_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        self.storage.storage_trie_cursor_with_tx(tx, hashed_address, max_block_number)
    }

    fn account_trie_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        self.storage.account_trie_cursor_with_tx(tx, max_block_number)
    }

    fn storage_hashed_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        self.storage.storage_hashed_cursor_with_tx(tx, hashed_address, max_block_number)
    }

    fn account_hashed_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        self.storage.account_hashed_cursor_with_tx(tx, max_block_number)
    }

    fn store_trie_updates(
        &self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        self.storage.store_trie_updates(block_ref, block_state_diff)
    }

    fn fetch_trie_updates(&self, block_number: u64) -> BaseProofsStorageResult<BlockStateDiff> {
        self.storage.fetch_trie_updates(block_number)
    }

    fn prune_earliest_state(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        self.storage.prune_earliest_state(new_earliest_block_ref)
    }

    fn unwind_history(&self, to: BlockWithParent) -> BaseProofsStorageResult<()> {
        self.storage.unwind_history(to)
    }

    fn replace_updates(
        &self,
        latest_common_block: BlockNumHash,
        blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
    ) -> BaseProofsStorageResult<()> {
        self.storage.replace_updates(latest_common_block, blocks_to_add)
    }

    fn set_earliest_block_number(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        self.storage.set_earliest_block_number(block_number, hash)
    }
}

impl BaseProofsInitialStateStore for TestRocksdbProofsStorage {
    fn initial_state_anchor(&self) -> BaseProofsStorageResult<InitialStateAnchor> {
        self.storage.initial_state_anchor()
    }

    fn set_initial_state_anchor(&self, anchor: BlockNumHash) -> BaseProofsStorageResult<()> {
        self.storage.set_initial_state_anchor(anchor)
    }

    fn store_account_branches(
        &self,
        account_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        self.storage.store_account_branches(account_nodes)
    }

    fn store_storage_branches(
        &self,
        hashed_address: B256,
        storage_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        self.storage.store_storage_branches(hashed_address, storage_nodes)
    }

    fn store_hashed_accounts(
        &self,
        accounts: Vec<(B256, Option<Account>)>,
    ) -> BaseProofsStorageResult<()> {
        self.storage.store_hashed_accounts(accounts)
    }

    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        storages: Vec<(B256, U256)>,
    ) -> BaseProofsStorageResult<()> {
        self.storage.store_hashed_storages(hashed_address, storages)
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        self.storage.commit_initial_state()
    }
}

const fn test_block(parent: B256, number: u64, hash_byte: u8) -> BlockWithParent {
    BlockWithParent::new(parent, NumHash::new(number, B256::repeat_byte(hash_byte)))
}

fn assert_account_at<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: &S,
    at: u64,
    key: B256,
    expected: Account,
) -> Result<(), BaseProofsStorageError> {
    let mut cursor = storage.account_hashed_cursor(at)?;
    assert_eq!(cursor.seek(key)?, Some((key, expected)));
    Ok(())
}

fn assert_account_branch_present<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: &S,
    at: u64,
    path: Nibbles,
) -> Result<(), BaseProofsStorageError> {
    let mut cursor = storage.account_trie_cursor(at)?;
    assert!(cursor.seek_exact(path)?.is_some());
    Ok(())
}

fn assert_account_branch_missing<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: &S,
    at: u64,
    path: Nibbles,
) -> Result<(), BaseProofsStorageError> {
    let mut cursor = storage.account_trie_cursor(at)?;
    assert!(cursor.seek_exact(path)?.is_none());
    Ok(())
}

fn assert_storage_at<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: &S,
    hashed_address: B256,
    at: u64,
    slot: B256,
    expected: U256,
) -> Result<(), BaseProofsStorageError> {
    let mut cursor = storage.storage_hashed_cursor(hashed_address, at)?;
    assert_eq!(cursor.seek(slot)?, Some((slot, expected)));
    Ok(())
}
