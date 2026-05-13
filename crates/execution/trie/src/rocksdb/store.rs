use std::{collections::BTreeMap, path::Path, sync::Arc};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use reth_db::{
    DatabaseError,
    table::{Compress, Decode, Decompress, Encode},
};
use reth_primitives_traits::Account;
use reth_trie_common::{
    BranchNodeCompact, HashedPostState, Nibbles, StoredNibbles,
    updates::{StorageTrieUpdates, TrieUpdates},
};
use rocksdb::{DB, Direction, IteratorMode, WriteBatch};

use super::{
    cf::{
        CF_ACCOUNT_TRIE_HISTORY, CF_BLOCK_CHANGE_SET, CF_HASHED_ACCOUNT_HISTORY,
        CF_HASHED_STORAGE_HISTORY, CF_PROOF_WINDOW, CF_STORAGE_TRIE_HISTORY, decode_composite_key,
        encode_composite_key, encode_key_floor, key_prefix_matches, open_rocksdb,
    },
    cursor::{RocksdbAccountCursor, RocksdbStorageCursor, RocksdbTrieCursor},
};
use crate::{
    BaseProofsInitialStateStore, BaseProofsStorageError, BaseProofsStorageResult, BaseProofsStore,
    BlockStateDiff,
    api::{InitialStateAnchor, InitialStateStatus, WriteCounts},
    db::{
        BlockNumberHash, ChangeSet, HashedStorageKey, MaybeDeleted, ProofWindowKey, StorageTrieKey,
        StorageValue,
    },
};

#[derive(Debug, Default, Clone)]
struct HistoryDeleteBatch {
    account_trie: Vec<(StoredNibbles, u64)>,
    storage_trie: Vec<(StorageTrieKey, u64)>,
    hashed_account: Vec<(B256, u64)>,
    hashed_storage: Vec<(HashedStorageKey, u64)>,
}

pub struct RocksdbProofsStorage {
    db: Arc<DB>,
}

impl std::fmt::Debug for RocksdbProofsStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksdbProofsStorage").finish_non_exhaustive()
    }
}

impl RocksdbProofsStorage {
    pub fn new(path: &Path) -> Result<Self, rocksdb::Error> {
        let db = open_rocksdb(path)?;
        Ok(Self { db: Arc::new(db) })
    }

    fn rocksdb_error(error: rocksdb::Error) -> BaseProofsStorageError {
        BaseProofsStorageError::DatabaseError(DatabaseError::Other(error.to_string()))
    }

    fn cf_not_found_error(cf_name: &str) -> BaseProofsStorageError {
        BaseProofsStorageError::DatabaseError(DatabaseError::Other(format!(
            "missing RocksDB column family: {cf_name}"
        )))
    }

    fn cf_handle<'a>(
        db: &'a DB,
        cf_name: &str,
    ) -> BaseProofsStorageResult<&'a rocksdb::ColumnFamily> {
        db.cf_handle(cf_name)
            .ok_or_else(|| Self::cf_not_found_error(cf_name))
    }

    fn block_key(block_number: u64) -> [u8; 8] {
        block_number.to_be_bytes()
    }

    fn decode_block_key(key: &[u8]) -> BaseProofsStorageResult<u64> {
        if key.len() != 8 {
            return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
        }
        Ok(u64::from_be_bytes(key.try_into().map_err(|_| DatabaseError::Decode)?))
    }

    fn get_proof_window_entry(&self, key: ProofWindowKey) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let cf = Self::cf_handle(&self.db, CF_PROOF_WINDOW)?;
        let value = self
            .db
            .get_pinned_cf(&cf, key.encode())
            .map_err(Self::rocksdb_error)?;

        value
            .map(|v| BlockNumberHash::decompress(v.as_ref()).map(|block| (block.number(), *block.hash())))
            .transpose()
            .map_err(Into::into)
    }

    fn set_proof_window_entry(
        &self,
        batch: &mut WriteBatch,
        key: ProofWindowKey,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let cf = Self::cf_handle(&self.db, CF_PROOF_WINDOW)?;
        batch.put_cf(&cf, key.encode(), BlockNumberHash::new(block_number, hash).compress());
        Ok(())
    }

    fn get_latest_block_number_hash(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        if let Some(latest) = self.get_proof_window_entry(ProofWindowKey::LatestBlock)? {
            return Ok(Some(latest));
        }
        self.get_proof_window_entry(ProofWindowKey::EarliestBlock)
    }

    fn write_batch(&self, batch: WriteBatch) -> BaseProofsStorageResult<()> {
        self.db.write(batch).map_err(Self::rocksdb_error)
    }

    fn put_history_value<T: Compress>(
        &self,
        batch: &mut WriteBatch,
        cf_name: &str,
        logical_key: &[u8],
        block_number: u64,
        value: Option<T>,
    ) -> BaseProofsStorageResult<()> {
        let cf = Self::cf_handle(&self.db, cf_name)?;
        let composite = encode_composite_key(logical_key, block_number);
        batch.put_cf(&cf, composite, MaybeDeleted(value).compress());
        Ok(())
    }

    fn get_exact_history_value<T: Decompress>(
        &self,
        cf_name: &str,
        logical_key: &[u8],
        block_number: u64,
    ) -> BaseProofsStorageResult<Option<Option<T>>> {
        let cf = Self::cf_handle(&self.db, cf_name)?;
        let composite = encode_composite_key(logical_key, block_number);
        let value = self.db.get_pinned_cf(&cf, composite).map_err(Self::rocksdb_error)?;
        value
            .map(|raw| MaybeDeleted::<T>::decompress(raw.as_ref()).map(|decoded| decoded.0))
            .transpose()
            .map_err(Into::into)
    }

    fn collect_live_storage_trie_paths(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Vec<Nibbles>> {
        let cf = Self::cf_handle(&self.db, CF_STORAGE_TRIE_HISTORY)?;
        let address_prefix = hashed_address.as_slice();
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek(encode_key_floor(address_prefix));

        let mut latest_by_key: BTreeMap<Vec<u8>, (u64, bool)> = BTreeMap::new();

        while iter.valid() {
            let Some(raw_key) = iter.key() else {
                break;
            };
            let (key_prefix, block_number) = decode_composite_key(raw_key);
            if !key_prefix.starts_with(address_prefix) {
                break;
            }

            if block_number <= max_block_number {
                let Some(raw_value) = iter.value() else {
                    return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
                };
                let MaybeDeleted(value) = MaybeDeleted::<BranchNodeCompact>::decompress(raw_value)?;

                let entry = latest_by_key.entry(key_prefix.to_vec()).or_insert((0, false));
                if block_number >= entry.0 {
                    *entry = (block_number, value.is_some());
                }
            }

            iter.next();
        }

        let mut result = Vec::new();
        for (key_prefix, (_, is_live)) in latest_by_key {
            if is_live {
                let key = StorageTrieKey::decode(&key_prefix)?;
                result.push(key.path.0);
            }
        }
        Ok(result)
    }

    fn collect_live_hashed_storage_slots(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Vec<B256>> {
        let cf = Self::cf_handle(&self.db, CF_HASHED_STORAGE_HISTORY)?;
        let address_prefix = hashed_address.as_slice();
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek(encode_key_floor(address_prefix));

        let mut latest_by_key: BTreeMap<Vec<u8>, (u64, bool)> = BTreeMap::new();

        while iter.valid() {
            let Some(raw_key) = iter.key() else {
                break;
            };
            let (key_prefix, block_number) = decode_composite_key(raw_key);
            if !key_prefix.starts_with(address_prefix) {
                break;
            }

            if block_number <= max_block_number {
                let Some(raw_value) = iter.value() else {
                    return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
                };
                let MaybeDeleted(value) = MaybeDeleted::<StorageValue>::decompress(raw_value)?;

                let entry = latest_by_key.entry(key_prefix.to_vec()).or_insert((0, false));
                if block_number >= entry.0 {
                    *entry = (block_number, value.is_some());
                }
            }

            iter.next();
        }

        let mut result = Vec::new();
        for (key_prefix, (_, is_live)) in latest_by_key {
            if is_live {
                let key = HashedStorageKey::decode(&key_prefix)?;
                result.push(key.hashed_storage_key);
            }
        }
        Ok(result)
    }

    fn store_trie_updates_for_block(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<ChangeSet> {
        let BlockStateDiff { sorted_trie_updates, sorted_post_state } = block_state_diff;

        let mut account_trie_keys = Vec::with_capacity(sorted_trie_updates.account_nodes_ref().len());
        for (path, node) in sorted_trie_updates.account_nodes_ref() {
            let key = StoredNibbles::from(*path);
            let encoded_key = key.clone().encode();
            self.put_history_value(
                batch,
                CF_ACCOUNT_TRIE_HISTORY,
                encoded_key.as_ref(),
                block_number,
                node.clone(),
            )?;
            account_trie_keys.push(key);
        }

        let mut hashed_account_keys = Vec::with_capacity(sorted_post_state.accounts.len());
        for (hashed_address, account) in &sorted_post_state.accounts {
            self.put_history_value(
                batch,
                CF_HASHED_ACCOUNT_HISTORY,
                hashed_address.as_slice(),
                block_number,
                *account,
            )?;
            hashed_account_keys.push(*hashed_address);
        }

        let mut storage_trie_keys = Vec::new();
        for (hashed_address, nodes) in sorted_trie_updates.storage_tries_ref() {
            if nodes.is_deleted {
                let existing = self.collect_live_storage_trie_paths(
                    *hashed_address,
                    block_number.saturating_sub(1),
                )?;
                let mut merged: BTreeMap<Nibbles, Option<BranchNodeCompact>> = BTreeMap::new();
                for path in existing {
                    merged.insert(path, None);
                }
                for (path, node) in nodes.storage_nodes_ref() {
                    merged.insert(*path, node.clone());
                }

                for (path, node) in merged {
                    let key = StorageTrieKey::new(*hashed_address, StoredNibbles(path));
                    let encoded_key = key.clone().encode();
                    self.put_history_value(
                        batch,
                        CF_STORAGE_TRIE_HISTORY,
                        encoded_key.as_ref(),
                        block_number,
                        node,
                    )?;
                    storage_trie_keys.push(key);
                }
                continue;
            }

            for (path, node) in nodes.storage_nodes_ref() {
                let key = StorageTrieKey::new(*hashed_address, StoredNibbles(*path));
                let encoded_key = key.clone().encode();
                self.put_history_value(
                    batch,
                    CF_STORAGE_TRIE_HISTORY,
                    encoded_key.as_ref(),
                    block_number,
                    node.clone(),
                )?;
                storage_trie_keys.push(key);
            }
        }

        let mut hashed_storage_keys = Vec::new();
        for (hashed_address, storage) in sorted_post_state.storages {
            if storage.is_wiped() {
                let existing =
                    self.collect_live_hashed_storage_slots(hashed_address, block_number.saturating_sub(1))?;
                let mut merged: BTreeMap<B256, Option<StorageValue>> = BTreeMap::new();
                for slot in existing {
                    merged.insert(slot, None);
                }
                for (slot, value) in storage.storage_slots_ref() {
                    merged.insert(*slot, Some(StorageValue(*value)));
                }

                for (slot, value) in merged {
                    let key = HashedStorageKey::new(hashed_address, slot);
                    let encoded_key = key.clone().encode();
                    self.put_history_value(
                        batch,
                        CF_HASHED_STORAGE_HISTORY,
                        encoded_key.as_ref(),
                        block_number,
                        value,
                    )?;
                    hashed_storage_keys.push(key);
                }
                continue;
            }

            for (slot, value) in storage.storage_slots_ref() {
                let key = HashedStorageKey::new(hashed_address, *slot);
                let encoded_key = key.clone().encode();
                self.put_history_value(
                    batch,
                    CF_HASHED_STORAGE_HISTORY,
                    encoded_key.as_ref(),
                    block_number,
                    Some(StorageValue(*value)),
                )?;
                hashed_storage_keys.push(key);
            }
        }

        Ok(ChangeSet {
            account_trie_keys,
            storage_trie_keys,
            hashed_account_keys,
            hashed_storage_keys,
        })
    }

    fn collect_history_ranged(
        &self,
        start_inclusive: u64,
        end_inclusive: Option<u64>,
    ) -> BaseProofsStorageResult<HistoryDeleteBatch> {
        let cf = Self::cf_handle(&self.db, CF_BLOCK_CHANGE_SET)?;
        let mut iter = self
            .db
            .iterator_cf(&cf, IteratorMode::From(&Self::block_key(start_inclusive), Direction::Forward));

        let mut history = HistoryDeleteBatch::default();
        while let Some(item) = iter.next() {
            let (raw_key, raw_value) = item.map_err(Self::rocksdb_error)?;
            let block_number = Self::decode_block_key(raw_key.as_ref())?;
            if end_inclusive.is_some_and(|end| block_number > end) {
                break;
            }

            let change_set = ChangeSet::decompress(raw_value.as_ref())?;
            history
                .account_trie
                .extend(change_set.account_trie_keys.into_iter().map(|k| (k, block_number)));
            history
                .storage_trie
                .extend(change_set.storage_trie_keys.into_iter().map(|k| (k, block_number)));
            history
                .hashed_account
                .extend(change_set.hashed_account_keys.into_iter().map(|k| (k, block_number)));
            history
                .hashed_storage
                .extend(change_set.hashed_storage_keys.into_iter().map(|k| (k, block_number)));
        }

        history.account_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.storage_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_account.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_storage.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));

        Ok(history)
    }

    fn get_initial_state_anchor_inner(&self) -> BaseProofsStorageResult<Option<BlockNumHash>> {
        let cf = Self::cf_handle(&self.db, CF_PROOF_WINDOW)?;
        let value = self
            .db
            .get_pinned_cf(&cf, ProofWindowKey::InitialStateAnchor.encode())
            .map_err(Self::rocksdb_error)?;
        value
            .map(|v| {
                BlockNumberHash::decompress(v.as_ref())
                    .map(|value| NumHash::new(value.number(), *value.hash()))
            })
            .transpose()
            .map_err(Into::into)
    }

    fn get_latest_account_trie_key(&self) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        let cf = Self::cf_handle(&self.db, CF_ACCOUNT_TRIE_HISTORY)?;
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek_to_last();
        if !iter.valid() {
            return Ok(None);
        }
        let Some(raw_key) = iter.key() else {
            return Ok(None);
        };
        let (logical_key, _) = decode_composite_key(raw_key);
        Ok(Some(StoredNibbles::decode(logical_key)?))
    }

    fn get_latest_storage_trie_key(&self) -> BaseProofsStorageResult<Option<StorageTrieKey>> {
        let cf = Self::cf_handle(&self.db, CF_STORAGE_TRIE_HISTORY)?;
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek_to_last();
        if !iter.valid() {
            return Ok(None);
        }
        let Some(raw_key) = iter.key() else {
            return Ok(None);
        };
        let (logical_key, _) = decode_composite_key(raw_key);
        Ok(Some(StorageTrieKey::decode(logical_key)?))
    }

    fn get_latest_hashed_account_key(&self) -> BaseProofsStorageResult<Option<B256>> {
        let cf = Self::cf_handle(&self.db, CF_HASHED_ACCOUNT_HISTORY)?;
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek_to_last();
        if !iter.valid() {
            return Ok(None);
        }
        let Some(raw_key) = iter.key() else {
            return Ok(None);
        };
        let (logical_key, _) = decode_composite_key(raw_key);
        if logical_key.len() != 32 {
            return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
        }
        Ok(Some(B256::from_slice(logical_key)))
    }

    fn get_latest_hashed_storage_key(&self) -> BaseProofsStorageResult<Option<HashedStorageKey>> {
        let cf = Self::cf_handle(&self.db, CF_HASHED_STORAGE_HISTORY)?;
        let mut iter = self.db.raw_iterator_cf(&cf);
        iter.seek_to_last();
        if !iter.valid() {
            return Ok(None);
        }
        let Some(raw_key) = iter.key() else {
            return Ok(None);
        };
        let (logical_key, _) = decode_composite_key(raw_key);
        Ok(Some(HashedStorageKey::decode(logical_key)?))
    }
}

impl BaseProofsStore for RocksdbProofsStorage {
    type StorageTrieCursor<'tx>
        = RocksdbTrieCursor
    where
        Self: 'tx;
    type AccountTrieCursor<'tx>
        = RocksdbTrieCursor
    where
        Self: 'tx;
    type StorageCursor<'tx>
        = RocksdbStorageCursor
    where
        Self: 'tx;
    type AccountHashedCursor<'tx>
        = RocksdbAccountCursor
    where
        Self: 'tx;

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.get_proof_window_entry(ProofWindowKey::EarliestBlock)
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.get_latest_block_number_hash()
    }

    fn storage_trie_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        Ok(RocksdbTrieCursor::new_storage(Arc::clone(&self.db), hashed_address, max_block_number))
    }

    fn account_trie_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        Ok(RocksdbTrieCursor::new_account(Arc::clone(&self.db), max_block_number))
    }

    fn storage_hashed_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>> {
        Ok(RocksdbStorageCursor::new(Arc::clone(&self.db), hashed_address, max_block_number))
    }

    fn account_hashed_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        Ok(RocksdbAccountCursor::new(Arc::clone(&self.db), max_block_number))
    }

    fn store_trie_updates(
        &self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let latest_block_hash =
            self.get_latest_block_number_hash()?.map_or(B256::ZERO, |(_, hash)| hash);
        if latest_block_hash != block_ref.parent {
            return Err(BaseProofsStorageError::OutOfOrder {
                block_number: block_ref.block.number,
                parent_block_hash: block_ref.parent,
                latest_block_hash,
            });
        }

        let mut batch = WriteBatch::default();
        let change_set =
            self.store_trie_updates_for_block(&mut batch, block_ref.block.number, block_state_diff)?;
        let counts = WriteCounts {
            account_trie_updates_written_total: change_set.account_trie_keys.len() as u64,
            storage_trie_updates_written_total: change_set.storage_trie_keys.len() as u64,
            hashed_accounts_written_total: change_set.hashed_account_keys.len() as u64,
            hashed_storages_written_total: change_set.hashed_storage_keys.len() as u64,
        };

        {
            let cf = Self::cf_handle(&self.db, CF_BLOCK_CHANGE_SET)?;
            batch.put_cf(&cf, Self::block_key(block_ref.block.number), change_set.compress());
        }

        self.set_proof_window_entry(
            &mut batch,
            ProofWindowKey::LatestBlock,
            block_ref.block.number,
            block_ref.block.hash,
        )?;

        self.write_batch(batch)?;

        Ok(counts)
    }

    fn fetch_trie_updates(&self, block_number: u64) -> BaseProofsStorageResult<BlockStateDiff> {
        let change_set = {
            let cf = Self::cf_handle(&self.db, CF_BLOCK_CHANGE_SET)?;
            let key = Self::block_key(block_number);
            let value = self
                .db
                .get_pinned_cf(&cf, key)
                .map_err(Self::rocksdb_error)?
                .ok_or(BaseProofsStorageError::NoChangeSetForBlock(block_number))?;
            ChangeSet::decompress(value.as_ref())?
        };

        let mut trie_updates = TrieUpdates::default();
        for key in change_set.account_trie_keys {
            let entry = self
                .get_exact_history_value::<BranchNodeCompact>(
                    CF_ACCOUNT_TRIE_HISTORY,
                    key.clone().encode().as_ref(),
                    block_number,
                )?
                .ok_or(BaseProofsStorageError::MissingAccountTrieHistory(key.0, block_number))?;

            if let Some(value) = entry {
                trie_updates.account_nodes.insert(key.0, value);
            } else {
                trie_updates.removed_nodes.insert(key.0);
            }
        }

        for key in change_set.storage_trie_keys {
            let entry = self
                .get_exact_history_value::<BranchNodeCompact>(
                    CF_STORAGE_TRIE_HISTORY,
                    key.clone().encode().as_ref(),
                    block_number,
                )?
                .ok_or(BaseProofsStorageError::MissingStorageTrieHistory(
                    key.hashed_address,
                    key.path.0,
                    block_number,
                ))?;

            let updates =
                trie_updates.storage_tries.entry(key.hashed_address).or_insert_with(StorageTrieUpdates::default);
            if let Some(value) = entry {
                updates.storage_nodes.insert(key.path.0, value);
            } else {
                updates.removed_nodes.insert(key.path.0);
            }
        }

        let mut post_state = HashedPostState::with_capacity(change_set.hashed_account_keys.len());
        for key in change_set.hashed_account_keys {
            let entry = self
                .get_exact_history_value::<Account>(
                    CF_HASHED_ACCOUNT_HISTORY,
                    key.as_slice(),
                    block_number,
                )?
                .ok_or(BaseProofsStorageError::MissingHashedAccountHistory(key, block_number))?;
            post_state.accounts.insert(key, entry);
        }

        for key in change_set.hashed_storage_keys {
            let entry = self
                .get_exact_history_value::<StorageValue>(
                    CF_HASHED_STORAGE_HISTORY,
                    key.clone().encode().as_ref(),
                    block_number,
                )?
                .ok_or(BaseProofsStorageError::MissingHashedStorageHistory {
                    hashed_address: key.hashed_address,
                    hashed_storage_key: key.hashed_storage_key,
                    block_number,
                })?;

            let storage = post_state.storages.entry(key.hashed_address).or_default();
            if let Some(value) = entry {
                storage.storage.insert(key.hashed_storage_key, value.0);
            } else {
                storage.storage.insert(key.hashed_storage_key, U256::ZERO);
            }
        }

        Ok(BlockStateDiff {
            sorted_trie_updates: trie_updates.into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        })
    }

    fn prune_earliest_state(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let target_block = new_earliest_block_ref.block.number;
        let Some((earliest_block, _)) = self.get_earliest_block_number()? else {
            return Ok(WriteCounts::default());
        };
        if earliest_block >= target_block {
            return Ok(WriteCounts::default());
        }

        let change_sets = self.collect_history_ranged(earliest_block + 1, Some(target_block))?;

        let mut acc_candidates: BTreeMap<StoredNibbles, u64> = BTreeMap::new();
        let mut storage_candidates: BTreeMap<StorageTrieKey, u64> = BTreeMap::new();
        let mut hashed_acc_candidates: BTreeMap<B256, u64> = BTreeMap::new();
        let mut hashed_storage_candidates: BTreeMap<HashedStorageKey, u64> = BTreeMap::new();

        for (k, block) in &change_sets.account_trie {
            acc_candidates
                .entry(k.clone())
                .and_modify(|curr| *curr = (*curr).max(*block))
                .or_insert(*block);
        }
        for (k, block) in &change_sets.storage_trie {
            storage_candidates
                .entry(k.clone())
                .and_modify(|curr| *curr = (*curr).max(*block))
                .or_insert(*block);
        }
        for (k, block) in &change_sets.hashed_account {
            hashed_acc_candidates
                .entry(*k)
                .and_modify(|curr| *curr = (*curr).max(*block))
                .or_insert(*block);
        }
        for (k, block) in &change_sets.hashed_storage {
            hashed_storage_candidates
                .entry(k.clone())
                .and_modify(|curr| *curr = (*curr).max(*block))
                .or_insert(*block);
        }

        let mut batch = WriteBatch::default();

        let mut acc_deleted = 0u64;
        for (key, survivor_block) in acc_candidates {
            let cf = Self::cf_handle(&self.db, CF_ACCOUNT_TRIE_HISTORY)?;
            let key_bytes = key.clone().encode();
            let mut iter = self.db.raw_iterator_cf(&cf);
            iter.seek(encode_key_floor(key_bytes.as_ref()));
            while iter.valid() {
                let Some(raw_key) = iter.key() else {
                    break;
                };
                if !key_prefix_matches(raw_key, key_bytes.as_ref()) {
                    break;
                }
                let (_, block_number) = decode_composite_key(raw_key);
                let composite = raw_key.to_vec();
                if block_number < survivor_block {
                    batch.delete_cf(&cf, composite);
                    acc_deleted += 1;
                } else if block_number == survivor_block {
                    let Some(raw_value) = iter.value() else {
                        return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
                    };
                    if MaybeDeleted::<BranchNodeCompact>::decompress(raw_value)?.0.is_none() {
                        batch.delete_cf(&cf, composite);
                        acc_deleted += 1;
                    }
                    break;
                } else {
                    break;
                }
                iter.next();
            }
        }

        let mut storage_deleted = 0u64;
        for (key, survivor_block) in storage_candidates {
            let cf = Self::cf_handle(&self.db, CF_STORAGE_TRIE_HISTORY)?;
            let key_bytes = key.clone().encode();
            let mut iter = self.db.raw_iterator_cf(&cf);
            iter.seek(encode_key_floor(key_bytes.as_ref()));
            while iter.valid() {
                let Some(raw_key) = iter.key() else {
                    break;
                };
                if !key_prefix_matches(raw_key, key_bytes.as_ref()) {
                    break;
                }
                let (_, block_number) = decode_composite_key(raw_key);
                let composite = raw_key.to_vec();
                if block_number < survivor_block {
                    batch.delete_cf(&cf, composite);
                    storage_deleted += 1;
                } else if block_number == survivor_block {
                    let Some(raw_value) = iter.value() else {
                        return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
                    };
                    if MaybeDeleted::<BranchNodeCompact>::decompress(raw_value)?.0.is_none() {
                        batch.delete_cf(&cf, composite);
                        storage_deleted += 1;
                    }
                    break;
                } else {
                    break;
                }
                iter.next();
            }
        }

        let mut hashed_account_deleted = 0u64;
        for (key, survivor_block) in hashed_acc_candidates {
            let cf = Self::cf_handle(&self.db, CF_HASHED_ACCOUNT_HISTORY)?;
            let mut iter = self.db.raw_iterator_cf(&cf);
            iter.seek(encode_key_floor(key.as_slice()));
            while iter.valid() {
                let Some(raw_key) = iter.key() else {
                    break;
                };
                if !key_prefix_matches(raw_key, key.as_slice()) {
                    break;
                }
                let (_, block_number) = decode_composite_key(raw_key);
                let composite = raw_key.to_vec();
                if block_number < survivor_block {
                    batch.delete_cf(&cf, composite);
                    hashed_account_deleted += 1;
                } else if block_number == survivor_block {
                    let Some(raw_value) = iter.value() else {
                        return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
                    };
                    if MaybeDeleted::<Account>::decompress(raw_value)?.0.is_none() {
                        batch.delete_cf(&cf, composite);
                        hashed_account_deleted += 1;
                    }
                    break;
                } else {
                    break;
                }
                iter.next();
            }
        }

        let mut hashed_storage_deleted = 0u64;
        for (key, survivor_block) in hashed_storage_candidates {
            let cf = Self::cf_handle(&self.db, CF_HASHED_STORAGE_HISTORY)?;
            let key_bytes = key.clone().encode();
            let mut iter = self.db.raw_iterator_cf(&cf);
            iter.seek(encode_key_floor(key_bytes.as_ref()));
            while iter.valid() {
                let Some(raw_key) = iter.key() else {
                    break;
                };
                if !key_prefix_matches(raw_key, key_bytes.as_ref()) {
                    break;
                }
                let (_, block_number) = decode_composite_key(raw_key);
                let composite = raw_key.to_vec();
                if block_number < survivor_block {
                    batch.delete_cf(&cf, composite);
                    hashed_storage_deleted += 1;
                } else if block_number == survivor_block {
                    let Some(raw_value) = iter.value() else {
                        return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Decode));
                    };
                    if MaybeDeleted::<StorageValue>::decompress(raw_value)?.0.is_none() {
                        batch.delete_cf(&cf, composite);
                        hashed_storage_deleted += 1;
                    }
                    break;
                } else {
                    break;
                }
                iter.next();
            }
        }

        {
            let cf = Self::cf_handle(&self.db, CF_BLOCK_CHANGE_SET)?;
            for block_number in (earliest_block + 1)..=target_block {
                batch.delete_cf(&cf, Self::block_key(block_number));
            }
        }

        self.set_proof_window_entry(
            &mut batch,
            ProofWindowKey::EarliestBlock,
            target_block,
            new_earliest_block_ref.block.hash,
        )?;

        self.write_batch(batch)?;

        Ok(WriteCounts {
            account_trie_updates_written_total: acc_deleted,
            storage_trie_updates_written_total: storage_deleted,
            hashed_accounts_written_total: hashed_account_deleted,
            hashed_storages_written_total: hashed_storage_deleted,
        })
    }

    fn unwind_history(&self, to: BlockWithParent) -> BaseProofsStorageResult<()> {
        let Some((earliest, _)) = self.get_earliest_block_number()? else {
            return Ok(());
        };
        let Some((latest, _)) = self.get_latest_block_number()? else {
            return Ok(());
        };

        if to.block.number > latest {
            return Ok(());
        }

        if to.block.number <= earliest {
            return Err(BaseProofsStorageError::UnwindBeyondEarliest {
                unwind_block_number: to.block.number,
                earliest_block_number: earliest,
            });
        }

        let history = self.collect_history_ranged(to.block.number, None)?;
        let mut batch = WriteBatch::default();

        {
            let cf = Self::cf_handle(&self.db, CF_ACCOUNT_TRIE_HISTORY)?;
            for (key, block) in history.account_trie {
                batch.delete_cf(&cf, encode_composite_key(key.encode().as_ref(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_STORAGE_TRIE_HISTORY)?;
            for (key, block) in history.storage_trie {
                batch.delete_cf(&cf, encode_composite_key(key.encode().as_ref(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_HASHED_ACCOUNT_HISTORY)?;
            for (key, block) in history.hashed_account {
                batch.delete_cf(&cf, encode_composite_key(key.as_slice(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_HASHED_STORAGE_HISTORY)?;
            for (key, block) in history.hashed_storage {
                batch.delete_cf(&cf, encode_composite_key(key.encode().as_ref(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_BLOCK_CHANGE_SET)?;
            let mut iter = self
                .db
                .iterator_cf(&cf, IteratorMode::From(&Self::block_key(to.block.number), Direction::Forward));
            while let Some(item) = iter.next() {
                let (raw_key, _) = item.map_err(Self::rocksdb_error)?;
                batch.delete_cf(&cf, raw_key.as_ref());
            }
        }

        self.set_proof_window_entry(
            &mut batch,
            ProofWindowKey::LatestBlock,
            to.block.number.saturating_sub(1),
            to.parent,
        )?;

        self.write_batch(batch)
    }

    fn replace_updates(
        &self,
        latest_common_block: BlockNumHash,
        mut blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
    ) -> BaseProofsStorageResult<()> {
        blocks_to_add.sort_unstable_by_key(|(block, _)| block.block.number);

        let history = self.collect_history_ranged(latest_common_block.number + 1, None)?;
        let mut batch = WriteBatch::default();

        {
            let cf = Self::cf_handle(&self.db, CF_ACCOUNT_TRIE_HISTORY)?;
            for (key, block) in history.account_trie {
                batch.delete_cf(&cf, encode_composite_key(key.encode().as_ref(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_STORAGE_TRIE_HISTORY)?;
            for (key, block) in history.storage_trie {
                batch.delete_cf(&cf, encode_composite_key(key.encode().as_ref(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_HASHED_ACCOUNT_HISTORY)?;
            for (key, block) in history.hashed_account {
                batch.delete_cf(&cf, encode_composite_key(key.as_slice(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_HASHED_STORAGE_HISTORY)?;
            for (key, block) in history.hashed_storage {
                batch.delete_cf(&cf, encode_composite_key(key.encode().as_ref(), block));
            }
        }
        {
            let cf = Self::cf_handle(&self.db, CF_BLOCK_CHANGE_SET)?;
            let mut iter = self.db.iterator_cf(
                &cf,
                IteratorMode::From(&Self::block_key(latest_common_block.number + 1), Direction::Forward),
            );
            while let Some(item) = iter.next() {
                let (raw_key, _) = item.map_err(Self::rocksdb_error)?;
                batch.delete_cf(&cf, raw_key.as_ref());
            }
        }

        self.set_proof_window_entry(
            &mut batch,
            ProofWindowKey::LatestBlock,
            latest_common_block.number,
            latest_common_block.hash,
        )?;
        self.write_batch(batch)?;

        for (block_with_parent, diff) in blocks_to_add {
            self.store_trie_updates(block_with_parent, diff)?;
        }

        Ok(())
    }

    fn set_earliest_block_number(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let mut batch = WriteBatch::default();
        self.set_proof_window_entry(&mut batch, ProofWindowKey::EarliestBlock, block_number, hash)?;
        self.write_batch(batch)
    }
}

impl BaseProofsInitialStateStore for RocksdbProofsStorage {
    fn initial_state_anchor(&self) -> BaseProofsStorageResult<InitialStateAnchor> {
        let Some(block) = self.get_initial_state_anchor_inner()? else {
            return Ok(InitialStateAnchor::default());
        };

        let completed = self.get_earliest_block_number()?.is_some();
        Ok(InitialStateAnchor {
            block: Some(block),
            status: if completed {
                InitialStateStatus::Completed
            } else {
                InitialStateStatus::InProgress
            },
            latest_account_trie_key: self.get_latest_account_trie_key()?,
            latest_storage_trie_key: self.get_latest_storage_trie_key()?,
            latest_hashed_account_key: self.get_latest_hashed_account_key()?,
            latest_hashed_storage_key: self.get_latest_hashed_storage_key()?,
        })
    }

    fn set_initial_state_anchor(&self, anchor: BlockNumHash) -> BaseProofsStorageResult<()> {
        if self.get_initial_state_anchor_inner()?.is_some() {
            return Err(BaseProofsStorageError::DatabaseError(DatabaseError::Other(
                "initial state anchor already exists".to_string(),
            )));
        }

        let mut batch = WriteBatch::default();
        self.set_proof_window_entry(
            &mut batch,
            ProofWindowKey::InitialStateAnchor,
            anchor.number,
            anchor.hash,
        )?;
        self.write_batch(batch)
    }

    fn store_account_branches(
        &self,
        mut account_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        if account_nodes.is_empty() {
            return Ok(());
        }

        account_nodes.sort_by_key(|(path, _)| *path);
        let mut batch = WriteBatch::default();
        for (path, node) in account_nodes {
            let key = StoredNibbles(path).encode();
            self.put_history_value(&mut batch, CF_ACCOUNT_TRIE_HISTORY, key.as_ref(), 0, node)?;
        }
        self.write_batch(batch)
    }

    fn store_storage_branches(
        &self,
        hashed_address: B256,
        mut storage_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        if storage_nodes.is_empty() {
            return Ok(());
        }

        storage_nodes.sort_by_key(|(path, _)| *path);
        let mut batch = WriteBatch::default();
        for (path, node) in storage_nodes {
            let key = StorageTrieKey::new(hashed_address, StoredNibbles(path)).encode();
            self.put_history_value(&mut batch, CF_STORAGE_TRIE_HISTORY, key.as_ref(), 0, node)?;
        }
        self.write_batch(batch)
    }

    fn store_hashed_accounts(
        &self,
        mut accounts: Vec<(B256, Option<Account>)>,
    ) -> BaseProofsStorageResult<()> {
        if accounts.is_empty() {
            return Ok(());
        }

        accounts.sort_by_key(|(address, _)| *address);
        let mut batch = WriteBatch::default();
        for (address, account) in accounts {
            self.put_history_value(
                &mut batch,
                CF_HASHED_ACCOUNT_HISTORY,
                address.as_slice(),
                0,
                account,
            )?;
        }
        self.write_batch(batch)
    }

    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        mut storages: Vec<(B256, U256)>,
    ) -> BaseProofsStorageResult<()> {
        if storages.is_empty() {
            return Ok(());
        }

        storages.sort_by_key(|(slot, _)| *slot);
        let mut batch = WriteBatch::default();
        for (slot, value) in storages {
            let key = HashedStorageKey::new(hashed_address, slot).encode();
            self.put_history_value(
                &mut batch,
                CF_HASHED_STORAGE_HISTORY,
                key.as_ref(),
                0,
                Some(StorageValue(value)),
            )?;
        }
        self.write_batch(batch)
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        let anchor = self.get_initial_state_anchor_inner()?.ok_or(BaseProofsStorageError::NoBlocksFound)?;
        self.set_earliest_block_number(anchor.number, anchor.hash)?;
        Ok(anchor)
    }
}
