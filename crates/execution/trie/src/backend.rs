//! One storage type for both proofs-history backends.
//!
//! The proofs `ExEx`, the proofs RPC overrides, and the initialization job are generic over the
//! store. Picking the MDBX or `RocksDB` backend at runtime used to instantiate all of that code
//! once per backend. [`ProofsBackend`] wraps either backend in one type, so that code is
//! instantiated once and each storage call dispatches with a two-way `match`.

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use reth_db::DatabaseError;
use reth_primitives_traits::Account;
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};
use reth_trie_common::{BranchNodeCompact, Nibbles};

use crate::{
    BaseProofsBatchSession, BaseProofsBatchStore, BaseProofsInitialStateStore,
    BaseProofsStorageError, BaseProofsStorageResult, BaseProofsStore, BlockStateDiff,
    MdbxProofsStorage, RocksdbBatchSession, RocksdbProofsStorage, StorageBranchEntries,
    api::{InitialStateAnchor, WriteCounts},
};

/// The proofs-history store backed by whichever of MDBX or `RocksDB` the node was configured with.
pub type BaseProofsBackend = ProofsBackend<MdbxProofsStorage, RocksdbProofsStorage>;

/// A value from the MDBX backend or the `RocksDB` backend.
///
/// Used for the store itself and for everything it hands out (transactions, cursors, and batch
/// sessions), which all delegate to the wrapped value.
#[derive(Debug)]
pub enum ProofsBackend<M, R> {
    /// MDBX backend.
    Mdbx(M),
    /// `RocksDB` backend.
    Rocksdb(R),
}

/// Delegates an expression to the wrapped value of either variant.
macro_rules! delegate {
    ($value:expr, $inner:ident => $body:expr) => {
        match $value {
            ProofsBackend::Mdbx($inner) => $body,
            ProofsBackend::Rocksdb($inner) => $body,
        }
    };
}

/// Like [`delegate!`], but re-wraps the result in the matching [`ProofsBackend`] variant.
macro_rules! delegate_wrap {
    ($value:expr, $inner:ident => $body:expr) => {
        match $value {
            ProofsBackend::Mdbx($inner) => $body.map(ProofsBackend::Mdbx),
            ProofsBackend::Rocksdb($inner) => $body.map(ProofsBackend::Rocksdb),
        }
    };
}

/// Like [`delegate_wrap!`], for methods that also take a transaction opened by this store.
macro_rules! delegate_with_tx {
    ($store:expr, $tx:expr, ($inner:ident, $inner_tx:ident) => $body:expr) => {
        match ($store, $tx) {
            (ProofsBackend::Mdbx($inner), ProofsBackend::Mdbx($inner_tx)) => {
                $body.map(ProofsBackend::Mdbx)
            }
            (ProofsBackend::Rocksdb($inner), ProofsBackend::Rocksdb($inner_tx)) => {
                $body.map(ProofsBackend::Rocksdb)
            }
            _ => Err(BaseProofsStorageError::BackendMismatch),
        }
    };
}

impl<M: TrieCursor, R: TrieCursor> TrieCursor for ProofsBackend<M, R> {
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        delegate!(self, c => c.seek_exact(key))
    }

    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        delegate!(self, c => c.seek(key))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        delegate!(self, c => c.next())
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        delegate!(self, c => c.current())
    }

    fn reset(&mut self) {
        delegate!(self, c => c.reset())
    }
}

impl<M: TrieStorageCursor, R: TrieStorageCursor> TrieStorageCursor for ProofsBackend<M, R> {
    fn set_hashed_address(&mut self, hashed_address: B256) {
        delegate!(self, c => c.set_hashed_address(hashed_address))
    }
}

impl<V, M, R> HashedCursor for ProofsBackend<M, R>
where
    V: core::fmt::Debug,
    M: HashedCursor<Value = V>,
    R: HashedCursor<Value = V>,
{
    type Value = V;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, V)>, DatabaseError> {
        delegate!(self, c => c.seek(key))
    }

    fn next(&mut self) -> Result<Option<(B256, V)>, DatabaseError> {
        delegate!(self, c => c.next())
    }

    fn reset(&mut self) {
        delegate!(self, c => c.reset())
    }
}

impl<V, M, R> HashedStorageCursor for ProofsBackend<M, R>
where
    V: core::fmt::Debug,
    M: HashedStorageCursor<Value = V>,
    R: HashedStorageCursor<Value = V>,
{
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        delegate!(self, c => c.is_storage_empty())
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        delegate!(self, c => c.set_hashed_address(hashed_address))
    }
}

impl<M: BaseProofsStore, R: BaseProofsStore> BaseProofsStore for ProofsBackend<M, R> {
    type StorageTrieCursor<'tx>
        = ProofsBackend<M::StorageTrieCursor<'tx>, R::StorageTrieCursor<'tx>>
    where
        Self: 'tx;
    type AccountTrieCursor<'tx>
        = ProofsBackend<M::AccountTrieCursor<'tx>, R::AccountTrieCursor<'tx>>
    where
        Self: 'tx;
    type StorageCursor<'tx>
        = ProofsBackend<M::StorageCursor<'tx>, R::StorageCursor<'tx>>
    where
        Self: 'tx;
    type AccountHashedCursor<'tx>
        = ProofsBackend<M::AccountHashedCursor<'tx>, R::AccountHashedCursor<'tx>>
    where
        Self: 'tx;
    type Tx<'tx>
        = ProofsBackend<M::Tx<'tx>, R::Tx<'tx>>
    where
        Self: 'tx;

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        delegate!(self, s => s.get_earliest_block_number())
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        delegate!(self, s => s.get_latest_block_number())
    }

    fn storage_trie_cursor<'tx>(
        &'tx self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        delegate_wrap!(self, s => s.storage_trie_cursor(hashed_address, max_block_number))
    }

    fn account_trie_cursor<'tx>(
        &'tx self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        delegate_wrap!(self, s => s.account_trie_cursor(max_block_number))
    }

    fn storage_hashed_cursor<'tx>(
        &'tx self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>> {
        delegate_wrap!(self, s => s.storage_hashed_cursor(hashed_address, max_block_number))
    }

    fn account_hashed_cursor<'tx>(
        &'tx self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        delegate_wrap!(self, s => s.account_hashed_cursor(max_block_number))
    }

    fn ro_tx<'tx>(&'tx self) -> BaseProofsStorageResult<Self::Tx<'tx>> {
        delegate_wrap!(self, s => s.ro_tx())
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
        delegate_with_tx!(self, tx, (s, tx) => {
            s.storage_trie_cursor_with_tx(tx, hashed_address, max_block_number)
        })
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
        delegate_with_tx!(self, tx, (s, tx) => s.account_trie_cursor_with_tx(tx, max_block_number))
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
        delegate_with_tx!(self, tx, (s, tx) => {
            s.storage_hashed_cursor_with_tx(tx, hashed_address, max_block_number)
        })
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
        delegate_with_tx!(self, tx, (s, tx) => {
            s.account_hashed_cursor_with_tx(tx, max_block_number)
        })
    }

    fn hashed_account_with_tx<'db>(
        &self,
        tx: &Self::Tx<'db>,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Option<Account>>
    where
        Self: 'db,
    {
        match (self, tx) {
            (Self::Mdbx(s), ProofsBackend::Mdbx(tx)) => {
                s.hashed_account_with_tx(tx, hashed_address, max_block_number)
            }
            (Self::Rocksdb(s), ProofsBackend::Rocksdb(tx)) => {
                s.hashed_account_with_tx(tx, hashed_address, max_block_number)
            }
            _ => Err(BaseProofsStorageError::BackendMismatch),
        }
    }

    fn hashed_storage_with_tx<'db>(
        &self,
        tx: &Self::Tx<'db>,
        hashed_address: B256,
        hashed_slot: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Option<U256>>
    where
        Self: 'db,
    {
        match (self, tx) {
            (Self::Mdbx(s), ProofsBackend::Mdbx(tx)) => {
                s.hashed_storage_with_tx(tx, hashed_address, hashed_slot, max_block_number)
            }
            (Self::Rocksdb(s), ProofsBackend::Rocksdb(tx)) => {
                s.hashed_storage_with_tx(tx, hashed_address, hashed_slot, max_block_number)
            }
            _ => Err(BaseProofsStorageError::BackendMismatch),
        }
    }

    fn store_trie_updates(
        &self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        delegate!(self, s => s.store_trie_updates(block_ref, block_state_diff))
    }

    fn fetch_trie_updates(&self, block_number: u64) -> BaseProofsStorageResult<BlockStateDiff> {
        delegate!(self, s => s.fetch_trie_updates(block_number))
    }

    fn prune_earliest_state(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        delegate!(self, s => s.prune_earliest_state(new_earliest_block_ref))
    }

    fn unwind_history(&self, to: BlockWithParent) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.unwind_history(to))
    }

    fn replace_updates(
        &self,
        latest_common_block: BlockNumHash,
        blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.replace_updates(latest_common_block, blocks_to_add))
    }

    fn set_earliest_block_number(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.set_earliest_block_number(block_number, hash))
    }
}

impl<M: BaseProofsBatchSession, R: BaseProofsBatchSession> BaseProofsBatchSession
    for ProofsBackend<M, R>
{
    type StorageTrieCursor<'a>
        = ProofsBackend<M::StorageTrieCursor<'a>, R::StorageTrieCursor<'a>>
    where
        Self: 'a;
    type AccountTrieCursor<'a>
        = ProofsBackend<M::AccountTrieCursor<'a>, R::AccountTrieCursor<'a>>
    where
        Self: 'a;
    type StorageCursor<'a>
        = ProofsBackend<M::StorageCursor<'a>, R::StorageCursor<'a>>
    where
        Self: 'a;
    type AccountHashedCursor<'a>
        = ProofsBackend<M::AccountHashedCursor<'a>, R::AccountHashedCursor<'a>>
    where
        Self: 'a;

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        delegate!(self, s => s.get_earliest_block_number())
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        delegate!(self, s => s.get_latest_block_number())
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'_>> {
        delegate_wrap!(self, s => s.storage_trie_cursor(hashed_address, max_block_number))
    }

    fn account_trie_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'_>> {
        delegate_wrap!(self, s => s.account_trie_cursor(max_block_number))
    }

    fn storage_hashed_cursor(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'_>> {
        delegate_wrap!(self, s => s.storage_hashed_cursor(hashed_address, max_block_number))
    }

    fn account_hashed_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'_>> {
        delegate_wrap!(self, s => s.account_hashed_cursor(max_block_number))
    }

    fn store_trie_updates(
        &mut self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        delegate!(self, s => s.store_trie_updates(block_ref, block_state_diff))
    }
}

impl BaseProofsBatchStore for BaseProofsBackend {
    type BatchSession<'a>
        = ProofsBackend<
        <MdbxProofsStorage as BaseProofsBatchStore>::BatchSession<'a>,
        RocksdbBatchSession<'a>,
    >
    where
        Self: 'a;

    /// Opens the backend's own session (see [`MdbxProofsStorage::begin_batch_session`] and
    /// [`RocksdbBatchSession::new`]), so atomicity is exactly the wrapped backend's.
    fn with_batch_session<T, F>(&self, f: F) -> BaseProofsStorageResult<T>
    where
        F: FnOnce(&mut Self::BatchSession<'_>) -> BaseProofsStorageResult<T>,
    {
        match self {
            Self::Mdbx(storage) => {
                let mut session = ProofsBackend::Mdbx(storage.begin_batch_session()?);
                let result = f(&mut session)?;
                if let ProofsBackend::Mdbx(session) = session {
                    session.commit()?;
                }
                Ok(result)
            }
            Self::Rocksdb(storage) => {
                let mut session = ProofsBackend::Rocksdb(RocksdbBatchSession::new(storage));
                f(&mut session)
            }
        }
    }
}

impl<M: BaseProofsInitialStateStore, R: BaseProofsInitialStateStore> BaseProofsInitialStateStore
    for ProofsBackend<M, R>
{
    fn initial_state_anchor(&self) -> BaseProofsStorageResult<InitialStateAnchor> {
        delegate!(self, s => s.initial_state_anchor())
    }

    fn set_initial_state_anchor(&self, anchor: BlockNumHash) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.set_initial_state_anchor(anchor))
    }

    fn store_account_branches(
        &self,
        account_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.store_account_branches(account_nodes))
    }

    fn store_storage_branches(
        &self,
        hashed_address: B256,
        storage_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.store_storage_branches(hashed_address, storage_nodes))
    }

    fn store_storage_branches_bulk(
        &self,
        entries: StorageBranchEntries,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.store_storage_branches_bulk(entries))
    }

    fn store_hashed_accounts(
        &self,
        accounts: Vec<(B256, Option<Account>)>,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.store_hashed_accounts(accounts))
    }

    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        storages: Vec<(B256, U256)>,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.store_hashed_storages(hashed_address, storages))
    }

    fn store_hashed_storages_bulk(
        &self,
        entries: Vec<(B256, Vec<(B256, U256)>)>,
    ) -> BaseProofsStorageResult<()> {
        delegate!(self, s => s.store_hashed_storages_bulk(entries))
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        delegate!(self, s => s.commit_initial_state())
    }
}

#[cfg(feature = "metrics")]
impl<M, R> reth_db::database_metrics::DatabaseMetrics for ProofsBackend<M, R>
where
    M: reth_db::database_metrics::DatabaseMetrics,
    R: reth_db::database_metrics::DatabaseMetrics,
{
    fn report_metrics(&self) {
        delegate!(self, s => s.report_metrics())
    }

    fn gauge_metrics(&self) -> Vec<(&'static str, f64, Vec<metrics::Label>)> {
        delegate!(self, s => s.gauge_metrics())
    }

    fn counter_metrics(&self) -> Vec<(&'static str, u64, Vec<metrics::Label>)> {
        delegate!(self, s => s.counter_metrics())
    }

    fn histogram_metrics(&self) -> Vec<(&'static str, f64, Vec<metrics::Label>)> {
        delegate!(self, s => s.histogram_metrics())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::{NumHash, eip1898::BlockWithParent};
    use tempfile::TempDir;

    use super::*;

    const fn block(num: u64) -> BlockWithParent {
        let parent = if num == 0 { B256::ZERO } else { B256::new([num as u8 - 1; 32]) };
        BlockWithParent::new(parent, NumHash::new(num, B256::new([num as u8; 32])))
    }

    fn seeded(backend: BaseProofsBackend) -> BaseProofsBackend {
        backend.set_earliest_block_number(0, block(0).block.hash).expect("set earliest");
        backend.store_trie_updates(block(0), BlockStateDiff::default()).expect("seed block 0");
        backend
    }

    fn mdbx(dir: &TempDir) -> BaseProofsBackend {
        seeded(ProofsBackend::Mdbx(MdbxProofsStorage::new(dir.path()).expect("mdbx")))
    }

    fn rocksdb(dir: &TempDir) -> BaseProofsBackend {
        seeded(ProofsBackend::Rocksdb(RocksdbProofsStorage::new(dir.path()).expect("rocksdb")))
    }

    fn latest(store: &BaseProofsBackend) -> u64 {
        store.get_latest_block_number().expect("latest").expect("some").0
    }

    #[test]
    fn batch_session_writes_through_either_backend() {
        for open in [mdbx, rocksdb] {
            let dir = TempDir::new().expect("tmp dir");
            let store = open(&dir);
            store
                .with_batch_session(|session| {
                    for n in 1..=3 {
                        session.store_trie_updates(block(n), BlockStateDiff::default())?;
                    }
                    assert_eq!(session.get_latest_block_number()?.expect("some").0, 3);
                    Ok(())
                })
                .expect("batch commit");
            assert_eq!(latest(&store), 3);
        }
    }

    #[test]
    fn mdbx_batch_session_rolls_back_on_error() {
        let dir = TempDir::new().expect("tmp dir");
        let store = mdbx(&dir);
        let result: BaseProofsStorageResult<()> = store.with_batch_session(|session| {
            session.store_trie_updates(block(1), BlockStateDiff::default())?;
            Err(BaseProofsStorageError::NoBlocksFound)
        });
        assert!(matches!(result, Err(BaseProofsStorageError::NoBlocksFound)));
        assert_eq!(latest(&store), 0, "aborted MDBX batch must not be visible");
    }

    #[test]
    fn transaction_from_the_other_backend_is_rejected() {
        let (mdbx_dir, rocksdb_dir) = (TempDir::new().expect("tmp"), TempDir::new().expect("tmp"));
        let (mdbx, rocksdb) = (mdbx(&mdbx_dir), rocksdb(&rocksdb_dir));
        let rocksdb_tx = rocksdb.ro_tx().expect("ro tx");
        assert!(matches!(
            mdbx.hashed_account_with_tx(&rocksdb_tx, B256::ZERO, 0),
            Err(BaseProofsStorageError::BackendMismatch)
        ));
        assert!(matches!(
            mdbx.account_trie_cursor_with_tx(&rocksdb_tx, 0),
            Err(BaseProofsStorageError::BackendMismatch)
        ));
    }
}
