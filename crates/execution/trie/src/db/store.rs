use std::{collections::BTreeSet, ops::RangeBounds, path::Path};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
#[cfg(feature = "metrics")]
use eyre::WrapErr;
#[cfg(feature = "metrics")]
use metrics::{Label, gauge};
use reth_db::{
    BlockNumberList, Database, DatabaseEnv, DatabaseError,
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO},
    mdbx::{DatabaseArguments, init_db_for},
    table::{DupSort, Table},
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, StorageEntry, ValueWithSubKey};
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};
use reth_trie_common::{
    BranchNodeCompact, HashedPostState, Nibbles, StorageTrieEntry, StoredNibbles,
    StoredNibblesSubKey,
    updates::{StorageTrieUpdates, TrieUpdates},
};
#[cfg(feature = "metrics")]
use tracing::error;

use super::{BlockNumberHash, ProofWindow, ProofWindowKey, Tables};
use crate::{
    BaseProofsStorageError,
    BaseProofsStorageError::NoBlocksFound,
    BaseProofsStorageResult, BaseProofsStore, BlockStateDiff,
    api::{
        BaseProofsBatchStore, BaseProofsInitialStateStore, InitialStateAnchor, InitialStateStatus,
        WriteCounts,
    },
    db::{
        MdbxBatchSession, MdbxV2AccountCursor, MdbxV2AccountTrieCursor, MdbxV2StorageCursor,
        MdbxV2StorageTrieCursor,
        models::{
            AccountTrieHistory, AccountTrieShardedKey, BlockChangeSet, BlockNumberHashedAddress,
            ChangeSet, HashedAccountBeforeTx, HashedAccountHistory, HashedAccountShardedKey,
            HashedStorageHistory, HashedStorageKey, HashedStorageShardedKey, StorageTrieHistory,
            StorageTrieKey, StorageTrieShardedKey, TrieChangeSetsEntry, V2AccountTrieChangeSets,
            V2AccountsTrie, V2AccountsTrieHistory, V2HashedAccountChangeSets, V2HashedAccounts,
            V2HashedAccountsHistory, V2HashedStorageChangeSets, V2HashedStorages,
            V2HashedStoragesHistory, V2ProofWindow, V2StorageTrieChangeSets, V2StoragesTrie,
            V2StoragesTrieHistory,
        },
    },
};

/// MDBX implementation of [`BaseProofsStore`].
#[derive(Debug)]
pub struct MdbxProofsStorage {
    env: DatabaseEnv,
}

struct ProofWindowValue {
    earliest: NumHash,
    latest: NumHash,
}

/// Preprocessed delete work for a prune range
#[derive(Debug, Default, Clone)]
struct HistoryDeleteBatch {
    account_trie: Vec<(<AccountTrieHistory as Table>::Key, u64)>,
    storage_trie: Vec<(<StorageTrieHistory as Table>::Key, u64)>,
    hashed_account: Vec<(<HashedAccountHistory as Table>::Key, u64)>,
    hashed_storage: Vec<(<HashedStorageHistory as Table>::Key, u64)>,
}

const NUM_OF_INDICES_IN_SHARD: usize = 2_000;

trait V2HistoryShardKey: Clone + Ord {
    type LogicalKey: Eq;

    fn logical_key(&self) -> Self::LogicalKey;

    fn with_highest_block(&self, highest_block_number: u64) -> Self;
}

impl V2HistoryShardKey for HashedAccountShardedKey {
    type LogicalKey = B256;

    fn logical_key(&self) -> Self::LogicalKey {
        self.0.key
    }

    fn with_highest_block(&self, highest_block_number: u64) -> Self {
        Self::new(self.0.key, highest_block_number)
    }
}

impl V2HistoryShardKey for HashedStorageShardedKey {
    type LogicalKey = (B256, B256);

    fn logical_key(&self) -> Self::LogicalKey {
        (self.hashed_address, self.sharded_key.key)
    }

    fn with_highest_block(&self, highest_block_number: u64) -> Self {
        Self::new(self.hashed_address, self.sharded_key.key, highest_block_number)
    }
}

impl V2HistoryShardKey for AccountTrieShardedKey {
    type LogicalKey = StoredNibbles;

    fn logical_key(&self) -> Self::LogicalKey {
        self.key.clone()
    }

    fn with_highest_block(&self, highest_block_number: u64) -> Self {
        Self::new(self.key.clone(), highest_block_number)
    }
}

impl V2HistoryShardKey for StorageTrieShardedKey {
    type LogicalKey = (B256, StoredNibbles);

    fn logical_key(&self) -> Self::LogicalKey {
        (self.hashed_address, self.key.clone())
    }

    fn with_highest_block(&self, highest_block_number: u64) -> Self {
        Self::new(self.hashed_address, self.key.clone(), highest_block_number)
    }
}

impl MdbxProofsStorage {
    /// Creates a new [`MdbxProofsStorage`] instance with the given path.
    pub fn new(path: &Path) -> Result<Self, BaseProofsStorageError> {
        let env = init_db_for::<_, Tables>(path, DatabaseArguments::default())
            .map_err(|e| DatabaseError::Other(format!("Failed to open database: {e}")))?;
        env.view(|tx| Self::validate_v2_schema(tx))??;
        Ok(Self { env })
    }

    fn validate_v2_schema(tx: &impl DbTx) -> BaseProofsStorageResult<()> {
        let mut cursor = tx.cursor_read::<V2ProofWindow>()?;
        if let Some((_, marker)) = cursor.seek_exact(ProofWindowKey::SchemaVersion)? {
            if marker.number() == 2 {
                return Ok(());
            }
            return Err(BaseProofsStorageError::UnsupportedSchemaVersion {
                actual: Some(marker.number()),
            });
        }

        if Self::proof_storage_has_entries(tx)? {
            return Err(BaseProofsStorageError::UnsupportedSchemaVersion { actual: None });
        }

        Ok(())
    }

    fn proof_storage_has_entries(tx: &impl DbTx) -> BaseProofsStorageResult<bool> {
        Ok(Self::table_has_entries::<ProofWindow>(tx)?
            || Self::table_has_entries::<BlockChangeSet>(tx)?
            || Self::table_has_entries::<AccountTrieHistory>(tx)?
            || Self::table_has_entries::<StorageTrieHistory>(tx)?
            || Self::table_has_entries::<HashedAccountHistory>(tx)?
            || Self::table_has_entries::<HashedStorageHistory>(tx)?
            || Self::table_has_entries::<V2ProofWindow>(tx)?
            || Self::table_has_entries::<V2HashedAccountsHistory>(tx)?
            || Self::table_has_entries::<V2HashedAccountChangeSets>(tx)?
            || Self::table_has_entries::<V2HashedAccounts>(tx)?
            || Self::table_has_entries::<V2HashedStoragesHistory>(tx)?
            || Self::table_has_entries::<V2HashedStorageChangeSets>(tx)?
            || Self::table_has_entries::<V2HashedStorages>(tx)?
            || Self::table_has_entries::<V2AccountsTrieHistory>(tx)?
            || Self::table_has_entries::<V2AccountTrieChangeSets>(tx)?
            || Self::table_has_entries::<V2AccountsTrie>(tx)?
            || Self::table_has_entries::<V2StoragesTrieHistory>(tx)?
            || Self::table_has_entries::<V2StorageTrieChangeSets>(tx)?
            || Self::table_has_entries::<V2StoragesTrie>(tx)?)
    }

    fn table_has_entries<T>(tx: &impl DbTx) -> BaseProofsStorageResult<bool>
    where
        T: Table,
    {
        Ok(tx.cursor_read::<T>()?.last()?.is_some())
    }

    pub(crate) fn inner_get_latest_block_number_hash(
        &self,
        tx: &impl DbTx,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let block = self.inner_get_block_number_hash(tx, ProofWindowKey::LatestBlock)?;
        if block.is_some() {
            return Ok(block);
        }

        self.inner_get_block_number_hash(tx, ProofWindowKey::EarliestBlock)
    }

    pub(crate) fn inner_get_earliest_block_number_hash(
        &self,
        tx: &impl DbTx,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.inner_get_block_number_hash(tx, ProofWindowKey::EarliestBlock)
    }

    fn inner_get_block_number_hash(
        &self,
        tx: &impl DbTx,
        key: ProofWindowKey,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let mut cursor = tx.cursor_read::<V2ProofWindow>()?;
        let value = cursor.seek_exact(key)?;
        Ok(value.map(|(_, val)| (val.number(), *val.hash())))
    }

    fn inner_get_proof_window(
        &self,
        tx: &impl DbTx,
    ) -> BaseProofsStorageResult<Option<ProofWindowValue>> {
        let mut cursor = tx.cursor_read::<V2ProofWindow>()?;

        let earliest = match cursor.seek_exact(ProofWindowKey::EarliestBlock)? {
            Some((_, val)) => NumHash::new(val.number(), *val.hash()),
            None => return Ok(None),
        };

        let latest = match cursor.seek_exact(ProofWindowKey::LatestBlock)? {
            Some((_, val)) => NumHash::new(val.number(), *val.hash()),
            None => earliest,
        };

        Ok(Some(ProofWindowValue { earliest, latest }))
    }

    fn set_earliest_block_number_hash(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let _ = self.env.update(|tx| {
            Self::inner_set_earliest_block_number(tx, block_number, hash)?;
            Ok::<(), DatabaseError>(())
        })?;
        Ok(())
    }

    /// Internal helper to set earliest block number hash within an existing transaction
    fn inner_set_earliest_block_number(
        tx: &(impl DbTxMut + DbTx),
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let mut v2_cursor = tx.cursor_write::<V2ProofWindow>()?;
        v2_cursor
            .upsert(ProofWindowKey::EarliestBlock, &BlockNumberHash::new(block_number, hash))?;
        v2_cursor.upsert(ProofWindowKey::SchemaVersion, &BlockNumberHash::new(2, B256::ZERO))?;
        Ok(())
    }

    /// Internal helper to set latest block number hash within an existing transaction
    fn inner_set_latest_block_number(
        tx: &(impl DbTxMut + DbTx),
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let mut v2_cursor = tx.cursor_write::<V2ProofWindow>()?;
        v2_cursor.upsert(ProofWindowKey::LatestBlock, &BlockNumberHash::new(block_number, hash))?;
        v2_cursor.upsert(ProofWindowKey::SchemaVersion, &BlockNumberHash::new(2, B256::ZERO))?;
        Ok(())
    }

    /// Collect versioned history over `block_range` using `BlockChangeSet`.
    fn collect_history_ranged(
        &self,
        tx: &impl DbTx,
        block_range: impl RangeBounds<u64>,
    ) -> BaseProofsStorageResult<HistoryDeleteBatch> {
        let mut history = HistoryDeleteBatch::default();
        let mut change_set_cursor = tx.cursor_read::<BlockChangeSet>()?;
        let mut walker = change_set_cursor.walk_range(block_range)?;

        while let Some(Ok((block_number, change_set))) = walker.next() {
            // Push (key, subkey=block_number) pairs
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

        // Sorting by tuple sorts by key first, then by block_number.
        history.account_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.storage_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_account.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_storage.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));

        Ok(history)
    }

    /// Delete versioned history over `block_range` using history batch.
    fn delete_history_ranged(
        &self,
        tx: &(impl DbTxMut + DbTx),
        block_range: impl RangeBounds<u64>,
        history: HistoryDeleteBatch,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let mut change_set_cursor = tx.cursor_write::<BlockChangeSet>()?;
        let mut walker = change_set_cursor.walk_range(block_range)?;

        while let Some(Ok((_, _))) = walker.next() {
            walker.delete_current()?;
        }

        Self::delete_v2_history_batch(tx, &history)?;

        Ok(WriteCounts {
            account_trie_updates_written_total: history.account_trie.len() as u64,
            storage_trie_updates_written_total: history.storage_trie.len() as u64,
            hashed_accounts_written_total: history.hashed_account.len() as u64,
            hashed_storages_written_total: history.hashed_storage.len() as u64,
        })
    }

    fn delete_v2_history_batch(
        tx: &(impl DbTxMut + DbTx),
        history: &HistoryDeleteBatch,
    ) -> BaseProofsStorageResult<()> {
        for (key, block_number) in &history.account_trie {
            let subkey = StoredNibblesSubKey::from(key.0);
            Self::delete_dup_current::<V2AccountTrieChangeSets>(tx, *block_number, subkey)?;
            Self::remove_v2_history::<V2AccountsTrieHistory>(
                tx,
                AccountTrieShardedKey::new(key.clone(), u64::MAX),
                *block_number,
            )?;
        }

        for (key, block_number) in &history.storage_trie {
            let subkey = StoredNibblesSubKey::from(key.path.0);
            Self::delete_dup_current::<V2StorageTrieChangeSets>(
                tx,
                BlockNumberHashedAddress((*block_number, key.hashed_address)),
                subkey,
            )?;
            Self::remove_v2_history::<V2StoragesTrieHistory>(
                tx,
                StorageTrieShardedKey::new(key.hashed_address, key.path.clone(), u64::MAX),
                *block_number,
            )?;
        }

        for (key, block_number) in &history.hashed_account {
            Self::delete_dup_current::<V2HashedAccountChangeSets>(tx, *block_number, *key)?;
            Self::remove_v2_history::<V2HashedAccountsHistory>(
                tx,
                HashedAccountShardedKey::new(*key, u64::MAX),
                *block_number,
            )?;
        }

        for (key, block_number) in &history.hashed_storage {
            Self::delete_dup_current::<V2HashedStorageChangeSets>(
                tx,
                BlockNumberHashedAddress((*block_number, key.hashed_address)),
                key.hashed_storage_key,
            )?;
            Self::remove_v2_history::<V2HashedStoragesHistory>(
                tx,
                HashedStorageShardedKey::new(key.hashed_address, key.hashed_storage_key, u64::MAX),
                *block_number,
            )?;
        }

        Ok(())
    }

    fn restore_v2_current_before_range(
        tx: &(impl DbTxMut + DbTx),
        history: &HistoryDeleteBatch,
    ) -> BaseProofsStorageResult<()> {
        let mut previous_account_trie_key = None;
        for (key, block_number) in &history.account_trie {
            if previous_account_trie_key.as_ref() == Some(key) {
                continue;
            }
            previous_account_trie_key = Some(key.clone());
            let subkey = StoredNibblesSubKey::from(key.0);
            let old = tx
                .cursor_dup_read::<V2AccountTrieChangeSets>()?
                .seek_by_key_subkey(*block_number, subkey.clone())?
                .filter(|entry| entry.nibbles == subkey)
                .and_then(|entry| entry.node);
            if let Some(node) = old {
                tx.cursor_write::<V2AccountsTrie>()?.upsert(key.clone(), &node)?;
            } else {
                Self::delete_simple_current::<V2AccountsTrie>(tx, key.clone())?;
            }
        }

        let mut previous_storage_trie_key = None;
        for (key, block_number) in &history.storage_trie {
            if previous_storage_trie_key.as_ref() == Some(key) {
                continue;
            }
            previous_storage_trie_key = Some(key.clone());
            let subkey = StoredNibblesSubKey::from(key.path.0);
            let old = tx
                .cursor_dup_read::<V2StorageTrieChangeSets>()?
                .seek_by_key_subkey(
                    BlockNumberHashedAddress((*block_number, key.hashed_address)),
                    subkey.clone(),
                )?
                .filter(|entry| entry.nibbles == subkey)
                .and_then(|entry| entry.node);
            if let Some(node) = old {
                tx.cursor_dup_write::<V2StoragesTrie>()?
                    .upsert(key.hashed_address, &StorageTrieEntry { nibbles: subkey, node })?;
            } else {
                Self::delete_dup_current::<V2StoragesTrie>(tx, key.hashed_address, subkey)?;
            }
        }

        let mut previous_hashed_account_key = None;
        for (key, block_number) in &history.hashed_account {
            if previous_hashed_account_key.as_ref() == Some(key) {
                continue;
            }
            previous_hashed_account_key = Some(*key);
            let old = tx
                .cursor_dup_read::<V2HashedAccountChangeSets>()?
                .seek_by_key_subkey(*block_number, *key)?
                .filter(|entry| entry.hashed_address == *key)
                .and_then(|entry| entry.info);
            if let Some(account) = old {
                tx.cursor_write::<V2HashedAccounts>()?.upsert(*key, &account)?;
            } else {
                Self::delete_simple_current::<V2HashedAccounts>(tx, *key)?;
            }
        }

        let mut previous_hashed_storage_key = None;
        for (key, block_number) in &history.hashed_storage {
            if previous_hashed_storage_key.as_ref() == Some(key) {
                continue;
            }
            previous_hashed_storage_key = Some(key.clone());
            let old = tx
                .cursor_dup_read::<V2HashedStorageChangeSets>()?
                .seek_by_key_subkey(
                    BlockNumberHashedAddress((*block_number, key.hashed_address)),
                    key.hashed_storage_key,
                )?
                .filter(|entry| entry.key == key.hashed_storage_key)
                .map_or(U256::ZERO, |entry| entry.value);
            if old.is_zero() {
                Self::delete_dup_current::<V2HashedStorages>(
                    tx,
                    key.hashed_address,
                    key.hashed_storage_key,
                )?;
            } else {
                tx.cursor_dup_write::<V2HashedStorages>()?.upsert(
                    key.hashed_address,
                    &StorageEntry { key: key.hashed_storage_key, value: old },
                )?;
            }
        }

        Ok(())
    }

    fn delete_simple_current<T>(
        tx: &(impl DbTxMut + DbTx),
        key: T::Key,
    ) -> BaseProofsStorageResult<()>
    where
        T: Table,
    {
        let mut cursor = tx.cursor_write::<T>()?;
        if cursor.seek_exact(key)?.is_some() {
            cursor.delete_current()?;
        }
        Ok(())
    }

    fn delete_dup_current<T>(
        tx: &(impl DbTxMut + DbTx),
        key: T::Key,
        subkey: T::SubKey,
    ) -> BaseProofsStorageResult<()>
    where
        T: Table + DupSort,
        T::Value: ValueWithSubKey<SubKey = T::SubKey>,
        T::SubKey: PartialEq + Clone,
    {
        let mut cursor = tx.cursor_dup_write::<T>()?;
        if let Some(value) = cursor.seek_by_key_subkey(key, subkey.clone())?
            && value.get_subkey() == subkey
        {
            cursor.delete_current()?;
        }
        Ok(())
    }

    fn store_trie_updates_for_block_v2_sidecar(
        &self,
        tx: &<DatabaseEnv as Database>::TXMut,
        block_number: u64,
        block_state_diff: &BlockStateDiff,
    ) -> BaseProofsStorageResult<ChangeSet> {
        let BlockStateDiff { sorted_trie_updates, sorted_post_state } = block_state_diff;
        let mut change_set = ChangeSet::default();

        for (path, node) in sorted_trie_updates.account_nodes_ref() {
            let current_key = StoredNibbles(*path);
            change_set.account_trie_keys.push(current_key.clone());
            let old = tx
                .cursor_read::<V2AccountsTrie>()?
                .seek_exact(current_key.clone())?
                .map(|(_, v)| v);

            tx.cursor_dup_write::<V2AccountTrieChangeSets>()?.upsert(
                block_number,
                &TrieChangeSetsEntry { nibbles: StoredNibblesSubKey::from(*path), node: old },
            )?;
            Self::append_v2_history::<V2AccountsTrieHistory>(
                tx,
                AccountTrieShardedKey::new(current_key.clone(), u64::MAX),
                block_number,
            )?;

            if let Some(node) = node {
                tx.cursor_write::<V2AccountsTrie>()?.upsert(current_key, node)?;
            } else {
                Self::delete_simple_current::<V2AccountsTrie>(tx, current_key)?;
            }
        }

        for (hashed_address, account) in &sorted_post_state.accounts {
            change_set.hashed_account_keys.push(*hashed_address);
            let old =
                tx.cursor_read::<V2HashedAccounts>()?.seek_exact(*hashed_address)?.map(|(_, v)| v);
            tx.cursor_dup_write::<V2HashedAccountChangeSets>()?
                .upsert(block_number, &HashedAccountBeforeTx::new(*hashed_address, old))?;
            Self::append_v2_history::<V2HashedAccountsHistory>(
                tx,
                HashedAccountShardedKey::new(*hashed_address, u64::MAX),
                block_number,
            )?;

            if let Some(account) = account {
                tx.cursor_write::<V2HashedAccounts>()?.upsert(*hashed_address, account)?;
            } else {
                Self::delete_simple_current::<V2HashedAccounts>(tx, *hashed_address)?;
            }
        }

        for (hashed_address, nodes) in sorted_trie_updates.storage_tries_ref() {
            let mut wiped_keys = BTreeSet::new();
            if nodes.is_deleted {
                let mut cursor = tx.cursor_dup_read::<V2StoragesTrie>()?;
                let mut old_entries = Vec::new();
                if let Some((key, entry)) = cursor.seek_exact(*hashed_address)?
                    && key == *hashed_address
                {
                    old_entries.push(entry);
                    while let Some((_, entry)) = cursor.next_dup()? {
                        old_entries.push(entry);
                    }
                }

                for entry in old_entries {
                    let key = StorageTrieKey::new(*hashed_address, StoredNibbles(entry.nibbles.0));
                    change_set.storage_trie_keys.push(key);
                    wiped_keys.insert(entry.nibbles.clone());
                    tx.cursor_dup_write::<V2StorageTrieChangeSets>()?.upsert(
                        BlockNumberHashedAddress((block_number, *hashed_address)),
                        &TrieChangeSetsEntry {
                            nibbles: entry.nibbles.clone(),
                            node: Some(entry.node),
                        },
                    )?;
                    Self::append_v2_history::<V2StoragesTrieHistory>(
                        tx,
                        StorageTrieShardedKey::new(
                            *hashed_address,
                            StoredNibbles(entry.nibbles.0),
                            u64::MAX,
                        ),
                        block_number,
                    )?;
                    Self::delete_dup_current::<V2StoragesTrie>(tx, *hashed_address, entry.nibbles)?;
                }
            }

            for (path, node) in nodes.storage_nodes_ref() {
                let nibbles = StoredNibblesSubKey::from(*path);
                let storage_key = StorageTrieKey::new(*hashed_address, StoredNibbles(*path));
                if !wiped_keys.contains(&nibbles) {
                    change_set.storage_trie_keys.push(storage_key);
                    let old = tx
                        .cursor_dup_read::<V2StoragesTrie>()?
                        .seek_by_key_subkey(*hashed_address, nibbles.clone())?
                        .filter(|entry| entry.nibbles == nibbles)
                        .map(|entry| entry.node);

                    tx.cursor_dup_write::<V2StorageTrieChangeSets>()?.upsert(
                        BlockNumberHashedAddress((block_number, *hashed_address)),
                        &TrieChangeSetsEntry { nibbles: nibbles.clone(), node: old },
                    )?;
                    Self::append_v2_history::<V2StoragesTrieHistory>(
                        tx,
                        StorageTrieShardedKey::new(*hashed_address, StoredNibbles(*path), u64::MAX),
                        block_number,
                    )?;
                }

                if let Some(node) = node {
                    tx.cursor_dup_write::<V2StoragesTrie>()?.upsert(
                        *hashed_address,
                        &StorageTrieEntry { nibbles, node: node.clone() },
                    )?;
                } else {
                    Self::delete_dup_current::<V2StoragesTrie>(tx, *hashed_address, nibbles)?;
                }
            }
        }

        for (hashed_address, storage) in &sorted_post_state.storages {
            let mut wiped_keys = BTreeSet::new();
            if storage.is_wiped() {
                let mut cursor = tx.cursor_dup_read::<V2HashedStorages>()?;
                let mut old_entries = Vec::new();
                if let Some((key, entry)) = cursor.seek_exact(*hashed_address)?
                    && key == *hashed_address
                {
                    old_entries.push(entry);
                    while let Some((_, entry)) = cursor.next_dup()? {
                        old_entries.push(entry);
                    }
                }

                for entry in old_entries {
                    let key = HashedStorageKey::new(*hashed_address, entry.key);
                    change_set.hashed_storage_keys.push(key);
                    wiped_keys.insert(entry.key);
                    tx.cursor_dup_write::<V2HashedStorageChangeSets>()?.upsert(
                        BlockNumberHashedAddress((block_number, *hashed_address)),
                        &entry,
                    )?;
                    Self::append_v2_history::<V2HashedStoragesHistory>(
                        tx,
                        HashedStorageShardedKey::new(*hashed_address, entry.key, u64::MAX),
                        block_number,
                    )?;
                    Self::delete_dup_current::<V2HashedStorages>(tx, *hashed_address, entry.key)?;
                }
            }

            for (hashed_storage_key, value) in storage.storage_slots_ref() {
                if !wiped_keys.contains(hashed_storage_key) {
                    change_set
                        .hashed_storage_keys
                        .push(HashedStorageKey::new(*hashed_address, *hashed_storage_key));
                    let old = tx
                        .cursor_dup_read::<V2HashedStorages>()?
                        .seek_by_key_subkey(*hashed_address, *hashed_storage_key)?
                        .filter(|entry| entry.key == *hashed_storage_key)
                        .map_or(U256::ZERO, |entry| entry.value);

                    tx.cursor_dup_write::<V2HashedStorageChangeSets>()?.upsert(
                        BlockNumberHashedAddress((block_number, *hashed_address)),
                        &StorageEntry { key: *hashed_storage_key, value: old },
                    )?;
                    Self::append_v2_history::<V2HashedStoragesHistory>(
                        tx,
                        HashedStorageShardedKey::new(
                            *hashed_address,
                            *hashed_storage_key,
                            u64::MAX,
                        ),
                        block_number,
                    )?;
                }

                if value.is_zero() {
                    Self::delete_dup_current::<V2HashedStorages>(
                        tx,
                        *hashed_address,
                        *hashed_storage_key,
                    )?;
                } else {
                    tx.cursor_dup_write::<V2HashedStorages>()?.upsert(
                        *hashed_address,
                        &StorageEntry { key: *hashed_storage_key, value: *value },
                    )?;
                }
            }
        }

        Ok(change_set)
    }

    fn append_v2_history<T>(
        tx: &(impl DbTxMut + DbTx),
        key: T::Key,
        block_number: u64,
    ) -> BaseProofsStorageResult<()>
    where
        T: Table<Value = BlockNumberList>,
        T::Key: V2HistoryShardKey,
    {
        let mut cursor = tx.cursor_write::<T>()?;
        let logical_key = key.logical_key();
        let first_key = key.with_highest_block(0);
        let mut row = cursor.seek(first_key)?;
        let mut old_keys = Vec::new();
        let mut block_numbers = BTreeSet::new();

        while let Some((history_key, list)) = row {
            if history_key.logical_key() != logical_key {
                break;
            }
            old_keys.push(history_key);
            block_numbers.extend(list.iter());
            row = cursor.next()?;
        }

        block_numbers.insert(block_number);
        for old_key in old_keys {
            if cursor.seek_exact(old_key)?.is_some() {
                cursor.delete_current()?;
            }
        }

        let blocks = block_numbers.into_iter().collect::<Vec<_>>();
        let chunk_count = blocks.len().div_ceil(NUM_OF_INDICES_IN_SHARD);
        for (index, chunk) in blocks.chunks(NUM_OF_INDICES_IN_SHARD).enumerate() {
            let highest_block_number = if index + 1 == chunk_count {
                u64::MAX
            } else {
                *chunk.last().expect("non-empty history shard")
            };
            cursor.upsert(
                key.with_highest_block(highest_block_number),
                &BlockNumberList::new_pre_sorted(chunk.iter().copied()),
            )?;
        }
        Ok(())
    }

    fn remove_v2_history<T>(
        tx: &(impl DbTxMut + DbTx),
        key: T::Key,
        block_number: u64,
    ) -> BaseProofsStorageResult<()>
    where
        T: Table<Value = BlockNumberList>,
        T::Key: V2HistoryShardKey,
    {
        let mut cursor = tx.cursor_write::<T>()?;
        let logical_key = key.logical_key();
        let first_key = key.with_highest_block(0);
        let mut row = cursor.seek(first_key)?;
        let mut old_keys = Vec::new();
        let mut block_numbers = BTreeSet::new();

        while let Some((history_key, list)) = row {
            if history_key.logical_key() != logical_key {
                break;
            }
            old_keys.push(history_key);
            block_numbers.extend(list.iter().filter(|changed_at| *changed_at != block_number));
            row = cursor.next()?;
        }

        for old_key in old_keys {
            if cursor.seek_exact(old_key)?.is_some() {
                cursor.delete_current()?;
            }
        }

        let blocks = block_numbers.into_iter().collect::<Vec<_>>();
        let chunk_count = blocks.len().div_ceil(NUM_OF_INDICES_IN_SHARD);
        for (index, chunk) in blocks.chunks(NUM_OF_INDICES_IN_SHARD).enumerate() {
            let highest_block_number = if index + 1 == chunk_count {
                u64::MAX
            } else {
                *chunk.last().expect("non-empty history shard")
            };
            cursor.upsert(
                key.with_highest_block(highest_block_number),
                &BlockNumberList::new_pre_sorted(chunk.iter().copied()),
            )?;
        }
        Ok(())
    }

    fn v2_history_contains<T>(
        tx: &impl DbTx,
        key: T::Key,
        block_number: u64,
    ) -> BaseProofsStorageResult<bool>
    where
        T: Table<Value = BlockNumberList>,
        T::Key: V2HistoryShardKey,
    {
        let logical_key = key.logical_key();
        let mut cursor = tx.cursor_read::<T>()?;
        let mut row = cursor.seek(key.with_highest_block(0))?;
        while let Some((history_key, list)) = row {
            if history_key.logical_key() != logical_key {
                return Ok(false);
            }
            if list.contains(block_number) {
                return Ok(true);
            }
            row = cursor.next()?;
        }
        Ok(false)
    }

    /// Append-only writer for a block: validates parent, persists diff (soft-delete=true),
    /// records a `BlockChangeSet`, and advances `ProofWindow::LatestBlock`.
    pub(crate) fn store_trie_updates_append_only(
        &self,
        tx: &<DatabaseEnv as Database>::TXMut,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let block_number = block_ref.block.number;

        // Check the latest stored block is the parent of the incoming block
        let latest_block_hash =
            self.inner_get_latest_block_number_hash(tx)?.map_or(B256::ZERO, |(_num, hash)| hash);

        if latest_block_hash != block_ref.parent {
            return Err(BaseProofsStorageError::OutOfOrder {
                block_number,
                parent_block_hash: block_ref.parent,
                latest_block_hash,
            });
        }

        let change_set =
            &self.store_trie_updates_for_block_v2_sidecar(tx, block_number, &block_state_diff)?;

        // Cursor for recording all changes made in this block for all history tables
        let mut change_set_cursor = tx.new_cursor::<BlockChangeSet>()?;
        change_set_cursor.append(block_number, change_set)?;

        // Update proof window's latest block
        Self::inner_set_latest_block_number(tx, block_number, block_ref.block.hash)?;

        Ok(WriteCounts {
            account_trie_updates_written_total: change_set.account_trie_keys.len() as u64,
            storage_trie_updates_written_total: change_set.storage_trie_keys.len() as u64,
            hashed_accounts_written_total: change_set.hashed_account_keys.len() as u64,
            hashed_storages_written_total: change_set.hashed_storage_keys.len() as u64,
        })
    }

    /// Return `BlockNumHash` for the initial state anchor.
    fn get_initial_state_anchor(&self) -> BaseProofsStorageResult<Option<BlockNumHash>> {
        self.env.view(|tx| {
            let mut cur = tx.cursor_read::<V2ProofWindow>()?;
            Ok(cur.seek_exact(ProofWindowKey::InitialStateAnchor)?.map(|(_k, v)| v.into()))
        })?
    }

    /// Return latest key for a table
    fn get_latest_key<T>(&self) -> BaseProofsStorageResult<Option<T::Key>>
    where
        T: Table,
    {
        self.env.view(|tx| {
            let mut cursor = tx.cursor_read::<T>()?;
            Ok(cursor.last()?.map(|(k, _)| k))
        })?
    }

    fn get_latest_storage_trie_current_key(
        &self,
    ) -> BaseProofsStorageResult<Option<StorageTrieKey>> {
        self.env.view(|tx| {
            let mut cursor = tx.cursor_dup_read::<V2StoragesTrie>()?;
            Ok(cursor.last()?.map(|(hashed_address, entry)| {
                StorageTrieKey::new(hashed_address, StoredNibbles(entry.nibbles.0))
            }))
        })?
    }

    fn get_latest_hashed_storage_current_key(
        &self,
    ) -> BaseProofsStorageResult<Option<HashedStorageKey>> {
        self.env.view(|tx| {
            let mut cursor = tx.cursor_dup_read::<V2HashedStorages>()?;
            Ok(cursor
                .last()?
                .map(|(hashed_address, entry)| HashedStorageKey::new(hashed_address, entry.key)))
        })?
    }
}

impl BaseProofsStore for MdbxProofsStorage {
    type StorageTrieCursor<'tx>
        = MdbxV2StorageTrieCursor
    where
        Self: 'tx;
    type AccountTrieCursor<'tx>
        = MdbxV2AccountTrieCursor
    where
        Self: 'tx;
    type StorageCursor<'tx>
        = MdbxV2StorageCursor
    where
        Self: 'tx;
    type AccountHashedCursor<'tx>
        = MdbxV2AccountCursor
    where
        Self: 'tx;
    type Tx = <DatabaseEnv as Database>::TX;

    fn ro_tx(&self) -> BaseProofsStorageResult<Self::Tx> {
        Ok(self.env.tx()?)
    }

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.env.view(|tx| self.inner_get_earliest_block_number_hash(tx))?
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.env.view(|tx| self.inner_get_latest_block_number_hash(tx))?
    }

    fn storage_trie_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        let tx = self.env.tx()?;
        MdbxV2StorageTrieCursor::new(&tx, max_block_number, hashed_address)
    }

    fn account_trie_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        let tx = self.env.tx()?;
        MdbxV2AccountTrieCursor::new(&tx, max_block_number)
    }

    fn storage_hashed_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>> {
        let tx = self.env.tx()?;
        MdbxV2StorageCursor::new(&tx, max_block_number, hashed_address)
    }

    fn account_hashed_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        let tx = self.env.tx()?;
        MdbxV2AccountCursor::new(&tx, max_block_number)
    }

    fn storage_trie_cursor_with_tx<'tx>(
        &self,
        tx: &'tx Self::Tx,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>>
    where
        Self: 'tx,
    {
        MdbxV2StorageTrieCursor::new(tx, max_block_number, hashed_address)
    }

    fn account_trie_cursor_with_tx<'tx>(
        &self,
        tx: &'tx Self::Tx,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>>
    where
        Self: 'tx,
    {
        MdbxV2AccountTrieCursor::new(tx, max_block_number)
    }

    fn storage_hashed_cursor_with_tx<'tx>(
        &self,
        tx: &'tx Self::Tx,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>>
    where
        Self: 'tx,
    {
        MdbxV2StorageCursor::new(tx, max_block_number, hashed_address)
    }

    fn account_hashed_cursor_with_tx<'tx>(
        &self,
        tx: &'tx Self::Tx,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>>
    where
        Self: 'tx,
    {
        MdbxV2AccountCursor::new(tx, max_block_number)
    }

    fn store_trie_updates(
        &self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        self.env
            .update(|tx| self.store_trie_updates_append_only(tx, block_ref, block_state_diff))?
    }

    fn fetch_trie_updates(&self, block_number: u64) -> BaseProofsStorageResult<BlockStateDiff> {
        self.env.view(|tx| {
            let mut change_set_cursor = tx.cursor_read::<BlockChangeSet>()?;
            let (_, change_set) = change_set_cursor
                .seek_exact(block_number)?
                .ok_or(BaseProofsStorageError::NoChangeSetForBlock(block_number))?;

            let mut trie_updates = TrieUpdates::default();
            let mut account_trie_cursor = MdbxV2AccountTrieCursor::new(tx, block_number)?;
            for key in change_set.account_trie_keys {
                if !Self::v2_history_contains::<V2AccountsTrieHistory>(
                    tx,
                    AccountTrieShardedKey::new(key.clone(), u64::MAX),
                    block_number,
                )? {
                    return Err(BaseProofsStorageError::MissingAccountTrieHistory(
                        key.0,
                        block_number,
                    ));
                }
                if let Some((_, value)) = account_trie_cursor.seek_exact(key.0)? {
                    trie_updates.account_nodes.insert(key.0, value);
                } else {
                    trie_updates.removed_nodes.insert(key.0);
                }
            }

            let mut storage_trie_cursor: Option<MdbxV2StorageTrieCursor> = None;
            for key in change_set.storage_trie_keys {
                if !Self::v2_history_contains::<V2StoragesTrieHistory>(
                    tx,
                    StorageTrieShardedKey::new(key.hashed_address, key.path.clone(), u64::MAX),
                    block_number,
                )? {
                    return Err(BaseProofsStorageError::MissingStorageTrieHistory(
                        key.hashed_address,
                        key.path.0,
                        block_number,
                    ));
                }
                let stu = trie_updates
                    .storage_tries
                    .entry(key.hashed_address)
                    .or_insert_with(StorageTrieUpdates::default);

                let cursor = match storage_trie_cursor.as_mut() {
                    Some(cursor) => {
                        cursor.set_hashed_address(key.hashed_address);
                        cursor
                    }
                    None => storage_trie_cursor.insert(MdbxV2StorageTrieCursor::new(
                        tx,
                        block_number,
                        key.hashed_address,
                    )?),
                };
                if let Some((_, value)) = cursor.seek_exact(key.path.0)? {
                    stu.storage_nodes.insert(key.path.0, value);
                } else {
                    stu.removed_nodes.insert(key.path.0);
                }
            }

            let mut post_state =
                HashedPostState::with_capacity(change_set.hashed_account_keys.len());
            let mut hashed_account_cursor = MdbxV2AccountCursor::new(tx, block_number)?;
            for key in change_set.hashed_account_keys {
                if !Self::v2_history_contains::<V2HashedAccountsHistory>(
                    tx,
                    HashedAccountShardedKey::new(key, u64::MAX),
                    block_number,
                )? {
                    return Err(BaseProofsStorageError::MissingHashedAccountHistory(
                        key,
                        block_number,
                    ));
                }
                let entry = hashed_account_cursor
                    .seek(key)?
                    .and_then(|(found_key, account)| (found_key == key).then_some(account));
                post_state.accounts.insert(key, entry);
            }

            let mut hashed_storage_cursor: Option<MdbxV2StorageCursor> = None;
            for key in change_set.hashed_storage_keys {
                if !Self::v2_history_contains::<V2HashedStoragesHistory>(
                    tx,
                    HashedStorageShardedKey::new(
                        key.hashed_address,
                        key.hashed_storage_key,
                        u64::MAX,
                    ),
                    block_number,
                )? {
                    return Err(BaseProofsStorageError::MissingHashedStorageHistory {
                        hashed_address: key.hashed_address,
                        hashed_storage_key: key.hashed_storage_key,
                        block_number,
                    });
                }
                let hs = post_state.storages.entry(key.hashed_address).or_default();
                let cursor = match hashed_storage_cursor.as_mut() {
                    Some(cursor) => {
                        cursor.set_hashed_address(key.hashed_address);
                        cursor
                    }
                    None => hashed_storage_cursor.insert(MdbxV2StorageCursor::new(
                        tx,
                        block_number,
                        key.hashed_address,
                    )?),
                };
                let value = cursor
                    .seek(key.hashed_storage_key)?
                    .and_then(|(found_key, value)| {
                        (found_key == key.hashed_storage_key).then_some(value)
                    })
                    .unwrap_or(U256::ZERO);
                hs.storage.insert(key.hashed_storage_key, value);
            }

            Ok(BlockStateDiff {
                sorted_trie_updates: trie_updates.into_sorted(),
                sorted_post_state: post_state.into_sorted(),
            })
        })?
    }

    /// Update the initial state with the provided diff.
    /// Prune all historical trie data till `new_earliest_block_number` (inclusive) using
    /// the [`BlockChangeSet`] index.
    ///
    /// Arguments:
    /// - `new_earliest_block_ref`: The new earliest block reference (with parent hash).
    /// - `diff`: The state diff to apply to the initial state (block 0). This diff represents all
    ///   the changes from the old earliest block to the new earliest block (inclusive).
    fn prune_earliest_state(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let target_block = new_earliest_block_ref.block.number;
        self.env.update(|tx| {
            let Some((earliest_block, _)) =
                self.inner_get_block_number_hash(tx, ProofWindowKey::EarliestBlock)?
            else {
                return Ok(WriteCounts::default());
            };

            if earliest_block >= target_block {
                return Ok(WriteCounts::default());
            }

            let range = (earliest_block + 1)..=target_block;
            let history = self.collect_history_ranged(tx, range.clone())?;
            let counts = self.delete_history_ranged(tx, range, history)?;

            Self::inner_set_earliest_block_number(
                tx,
                target_block,
                new_earliest_block_ref.block.hash,
            )?;

            Ok(counts)
        })?
    }

    /// Unwind the historical state to `unwind_upto_block` (inclusive), deleting all history
    /// starting from provided block. Also updates the `ProofWindow::LatestBlock` to parent of
    /// `unwind_upto_block`.
    fn unwind_history(&self, to: BlockWithParent) -> BaseProofsStorageResult<()> {
        let history_to_delete =
            self.env.view(|tx| self.collect_history_ranged(tx, to.block.number..))??;

        self.env.update(|tx| {
            let proof_window = match self.inner_get_proof_window(tx)? {
                Some(pw) => pw,
                None => return Ok(()), // Nothing to unwind
            };

            if to.block.number > proof_window.latest.number {
                return Ok(()); // Nothing to unwind
            }

            if to.block.number <= proof_window.earliest.number {
                return Err(BaseProofsStorageError::UnwindBeyondEarliest {
                    unwind_block_number: to.block.number,
                    earliest_block_number: proof_window.earliest.number,
                });
            }

            Self::restore_v2_current_before_range(tx, &history_to_delete)?;
            self.delete_history_ranged(tx, to.block.number.., history_to_delete)?;

            let new_latest_block =
                BlockNumberHash::new(to.block.number.saturating_sub(1), to.parent);

            // Update proof window's Latest block
            Self::inner_set_latest_block_number(
                tx,
                new_latest_block.number(),
                *new_latest_block.hash(),
            )?;

            Ok(())
        })?
    }

    fn replace_updates(
        &self,
        latest_common_block: BlockNumHash,
        mut blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
    ) -> BaseProofsStorageResult<()> {
        // Sort the vec list by block number
        blocks_to_add.sort_unstable_by_key(|(bwp, _)| bwp.block.number);

        let history_to_delete = self
            .env
            .view(|tx| self.collect_history_ranged(tx, latest_common_block.number + 1..))??;

        self.env.update(|tx| {
            Self::restore_v2_current_before_range(tx, &history_to_delete)?;
            self.delete_history_ranged(tx, latest_common_block.number + 1.., history_to_delete)?;

            // Update the ProofWindow Latest Block to latest_common_block so we can perform
            // `store_trie_updates_append_only`.
            Self::inner_set_latest_block_number(
                tx,
                latest_common_block.number,
                latest_common_block.hash,
            )?;

            // Apply the new history
            for (block_with_parent, diff) in blocks_to_add {
                self.store_trie_updates_append_only(tx, block_with_parent, diff)?;
            }
            Ok(())
        })?
    }

    fn set_earliest_block_number(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        self.set_earliest_block_number_hash(block_number, hash)
    }
}

impl BaseProofsBatchStore for MdbxProofsStorage {
    type BatchSession<'a>
        = MdbxBatchSession<'a>
    where
        Self: 'a;

    fn with_batch_session<R, F>(&self, f: F) -> BaseProofsStorageResult<R>
    where
        F: FnOnce(&mut Self::BatchSession<'_>) -> BaseProofsStorageResult<R>,
    {
        let tx = self.env.tx_mut()?;
        let mut session = MdbxBatchSession::new(self, tx);
        match f(&mut session) {
            Ok(result) => {
                session.commit()?;
                Ok(result)
            }
            Err(err) => Err(err),
        }
    }
}

impl BaseProofsInitialStateStore for MdbxProofsStorage {
    fn initial_state_anchor(&self) -> BaseProofsStorageResult<InitialStateAnchor> {
        // 1) NotStarted: no anchor row
        let Some(block) = self.get_initial_state_anchor()? else {
            return Ok(InitialStateAnchor::default());
        };

        // 2) Completed: anchor exists + earliest is set
        let completed = self.get_earliest_block_number()?.is_some();

        // 3) InProgress / Completed: populate details
        Ok(InitialStateAnchor {
            block: Some(block),
            status: if completed {
                InitialStateStatus::Completed
            } else {
                InitialStateStatus::InProgress
            },
            latest_account_trie_key: self.get_latest_key::<V2AccountsTrie>()?,
            latest_storage_trie_key: self.get_latest_storage_trie_current_key()?,
            latest_hashed_account_key: self.get_latest_key::<V2HashedAccounts>()?,
            latest_hashed_storage_key: self.get_latest_hashed_storage_current_key()?,
        })
    }

    fn set_initial_state_anchor(&self, anchor: BlockNumHash) -> BaseProofsStorageResult<()> {
        self.env.update(|tx| {
            let mut v2_cur = tx.cursor_write::<V2ProofWindow>()?;
            v2_cur.insert(ProofWindowKey::InitialStateAnchor, &anchor.into())?;
            v2_cur.upsert(ProofWindowKey::SchemaVersion, &BlockNumberHash::new(2, B256::ZERO))?;
            Ok(())
        })?
    }

    fn store_account_branches(
        &self,
        account_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        let mut account_nodes = account_nodes;
        if account_nodes.is_empty() {
            return Ok(());
        }

        account_nodes.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            for (path, node) in account_nodes {
                let key = StoredNibbles(path);
                if let Some(node) = node {
                    tx.cursor_write::<V2AccountsTrie>()?.upsert(key, &node)?;
                } else {
                    Self::delete_simple_current::<V2AccountsTrie>(tx, key)?;
                }
            }
            Ok(())
        })?
    }

    fn store_storage_branches(
        &self,
        hashed_address: B256,
        storage_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        let mut storage_nodes = storage_nodes;
        if storage_nodes.is_empty() {
            return Ok(());
        }

        storage_nodes.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            for (path, node) in storage_nodes {
                let nibbles = StoredNibblesSubKey::from(path);
                if let Some(node) = node {
                    tx.cursor_dup_write::<V2StoragesTrie>()?
                        .upsert(hashed_address, &StorageTrieEntry { nibbles, node })?;
                } else {
                    Self::delete_dup_current::<V2StoragesTrie>(tx, hashed_address, nibbles)?;
                }
            }
            Ok(())
        })?
    }

    fn store_hashed_accounts(
        &self,
        accounts: Vec<(B256, Option<Account>)>,
    ) -> BaseProofsStorageResult<()> {
        let mut accounts = accounts;
        if accounts.is_empty() {
            return Ok(());
        }

        // sort the accounts by key to ensure insertion is efficient
        accounts.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            for (hashed_address, account) in accounts {
                if let Some(account) = account {
                    tx.cursor_write::<V2HashedAccounts>()?.upsert(hashed_address, &account)?;
                } else {
                    Self::delete_simple_current::<V2HashedAccounts>(tx, hashed_address)?;
                }
            }
            Ok(())
        })?
    }

    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        storages: Vec<(B256, U256)>,
    ) -> BaseProofsStorageResult<()> {
        let mut storages = storages;
        if storages.is_empty() {
            return Ok(());
        }

        // sort the storages by key to ensure insertion is efficient
        storages.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            for (key, value) in storages {
                if value.is_zero() {
                    Self::delete_dup_current::<V2HashedStorages>(tx, hashed_address, key)?;
                } else {
                    tx.cursor_dup_write::<V2HashedStorages>()?
                        .upsert(hashed_address, &StorageEntry { key, value })?;
                }
            }
            Ok(())
        })?
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        let anchor = self.get_initial_state_anchor()?.ok_or(NoBlocksFound)?;
        self.set_earliest_block_number(anchor.number, anchor.hash)?;
        self.env
            .update(|tx| Self::inner_set_latest_block_number(tx, anchor.number, anchor.hash))??;
        Ok(anchor)
    }
}

/// This implementation is copied from the
/// [`DatabaseMetrics`](reth_db::database_metrics::DatabaseMetrics) implementation for
/// [`DatabaseEnv`]. As the implementation hard-coded the table name, we need to reimplement it.
#[cfg(feature = "metrics")]
impl reth_db::database_metrics::DatabaseMetrics for MdbxProofsStorage {
    fn report_metrics(&self) {
        for (name, value, labels) in self.gauge_metrics() {
            gauge!(name, labels).set(value);
        }
    }

    fn gauge_metrics(&self) -> Vec<(&'static str, f64, Vec<Label>)> {
        let mut metrics = Vec::new();

        let _ = self
            .env
            .view(|tx| {
                for table in Tables::ALL.iter().map(Tables::name) {
                    let table_db =
                        tx.inner().open_db(Some(table)).wrap_err("Could not open db.")?;

                    let stats = tx
                        .inner()
                        .db_stat(table_db.dbi())
                        .wrap_err(format!("Could not find table: {table}"))?;

                    let page_size = stats.page_size() as usize;
                    let leaf_pages = stats.leaf_pages();
                    let branch_pages = stats.branch_pages();
                    let overflow_pages = stats.overflow_pages();
                    let num_pages = leaf_pages + branch_pages + overflow_pages;
                    let table_size = page_size * num_pages;
                    let entries = stats.entries();

                    metrics.push((
                        "base_proof_storage.table_size",
                        table_size as f64,
                        vec![Label::new("table", table)],
                    ));
                    metrics.push((
                        "base_proof_storage.table_pages",
                        leaf_pages as f64,
                        vec![Label::new("table", table), Label::new("type", "leaf")],
                    ));
                    metrics.push((
                        "base_proof_storage.table_pages",
                        branch_pages as f64,
                        vec![Label::new("table", table), Label::new("type", "branch")],
                    ));
                    metrics.push((
                        "base_proof_storage.table_pages",
                        overflow_pages as f64,
                        vec![Label::new("table", table), Label::new("type", "overflow")],
                    ));
                    metrics.push((
                        "base_proof_storage.table_entries",
                        entries as f64,
                        vec![Label::new("table", table)],
                    ));
                }

                Ok::<(), eyre::Report>(())
            })
            .map_err(|error| error!(%error, "Failed to read db table stats"));

        if let Ok(freelist) =
            self.env.freelist().map_err(|error| error!(%error, "Failed to read db.freelist"))
        {
            metrics.push(("base_proof_storage.freelist", freelist as f64, vec![]));
        }

        if let Ok(stat) = self.env.stat().map_err(|error| error!(%error, "Failed to read db.stat"))
        {
            metrics.push(("base_proof_storage.page_size", stat.page_size() as f64, vec![]));
        }

        metrics.push((
            "base_proof_storage.timed_out_not_aborted_transactions",
            self.env.timed_out_not_aborted_transactions() as f64,
            vec![],
        ));

        metrics
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::NumHash;
    use alloy_primitives::B256;
    use reth_db::{
        DatabaseError,
        cursor::{DbCursorRO, DbDupCursorRO},
        transaction::{DbTx, DbTxMut},
    };
    use reth_trie::{
        BranchNodeCompact, HashedPostState, HashedStorage, Nibbles, StoredNibbles,
        updates::{StorageTrieUpdates, TrieUpdates},
    };
    use tempfile::TempDir;

    use super::*;
    use crate::db::models::{
        AccountTrieHistory, HashedAccountHistory, ProofWindow, StorageTrieHistory,
    };

    fn block(number: u64, parent: B256) -> BlockWithParent {
        debug_assert!(u8::try_from(number).is_ok());
        BlockWithParent::new(parent, NumHash::new(number, B256::with_last_byte(number as u8)))
    }

    fn account_diff(addr: B256, account: Option<Account>) -> BlockStateDiff {
        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(addr, account);
        BlockStateDiff { sorted_post_state: post_state.into_sorted(), ..Default::default() }
    }

    fn storage_diff(addr: B256, slot: B256, value: U256) -> BlockStateDiff {
        let mut storage = HashedStorage::default();
        storage.storage.insert(slot, value);
        let mut post_state = HashedPostState::default();
        post_state.storages.insert(addr, storage);
        BlockStateDiff { sorted_post_state: post_state.into_sorted(), ..Default::default() }
    }

    fn assert_no_v1_history(store: &MdbxProofsStorage) {
        let tx = store.env.tx().expect("ro tx");
        assert!(tx.cursor_read::<AccountTrieHistory>().unwrap().last().unwrap().is_none());
        assert!(tx.cursor_read::<StorageTrieHistory>().unwrap().last().unwrap().is_none());
        assert!(tx.cursor_read::<HashedAccountHistory>().unwrap().last().unwrap().is_none());
        assert!(tx.cursor_read::<HashedStorageHistory>().unwrap().last().unwrap().is_none());
        assert!(tx.cursor_read::<ProofWindow>().unwrap().last().unwrap().is_none());
    }

    #[test]
    fn initial_state_writes_v2_current_tables_only() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let account_addr = B256::from([0x11; 32]);
        let storage_addr = B256::from([0x22; 32]);
        let slot = B256::from([0x33; 32]);
        let account = Account { nonce: 2, balance: U256::from(100), ..Default::default() };
        let account_path = Nibbles::from_nibbles_unchecked([0x01, 0x02]);
        let storage_path = Nibbles::from_nibbles_unchecked([0x03, 0x04]);
        let account_node = BranchNodeCompact::new(0b1, 0, 0, vec![], Some(B256::random()));
        let storage_node = BranchNodeCompact::new(0b10, 0, 0, vec![], Some(B256::random()));

        store.store_hashed_accounts(vec![(account_addr, Some(account))]).expect("accounts");
        store.store_hashed_storages(storage_addr, vec![(slot, U256::from(7))]).expect("storage");
        store
            .store_account_branches(vec![(account_path, Some(account_node.clone()))])
            .expect("trie");
        store
            .store_storage_branches(storage_addr, vec![(storage_path, Some(storage_node.clone()))])
            .expect("storage trie");

        let tx = store.env.tx().expect("ro tx");
        assert_eq!(
            tx.cursor_read::<V2HashedAccounts>()
                .unwrap()
                .seek_exact(account_addr)
                .unwrap()
                .map(|(_, account)| account),
            Some(account),
        );
        assert_eq!(
            tx.cursor_dup_read::<V2HashedStorages>()
                .unwrap()
                .seek_by_key_subkey(storage_addr, slot)
                .unwrap()
                .map(|entry| entry.value),
            Some(U256::from(7)),
        );
        assert_eq!(
            tx.cursor_read::<V2AccountsTrie>()
                .unwrap()
                .seek_exact(StoredNibbles::from(account_path))
                .unwrap()
                .map(|(_, node)| node),
            Some(account_node),
        );
        assert_eq!(
            tx.cursor_dup_read::<V2StoragesTrie>()
                .unwrap()
                .seek_by_key_subkey(storage_addr, StoredNibblesSubKey::from(storage_path))
                .unwrap()
                .map(|entry| entry.node),
            Some(storage_node),
        );
        drop(tx);
        assert_no_v1_history(&store);
    }

    #[test]
    fn initial_state_resume_uses_v2_current_keys() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let anchor = BlockNumHash::new(10, B256::from([0xAA; 32]));
        let account_addr = B256::from([0x11; 32]);
        let storage_addr = B256::from([0x22; 32]);
        let slot = B256::from([0x33; 32]);
        let account_path = Nibbles::from_nibbles_unchecked([0x01, 0x02]);
        let storage_path = Nibbles::from_nibbles_unchecked([0x03, 0x04]);

        store.set_initial_state_anchor(anchor).expect("anchor");
        store
            .store_hashed_accounts(vec![(account_addr, Some(Account::default()))])
            .expect("accounts");
        store.store_hashed_storages(storage_addr, vec![(slot, U256::from(1))]).expect("storage");
        store
            .store_account_branches(vec![(account_path, Some(BranchNodeCompact::default()))])
            .expect("trie");
        store
            .store_storage_branches(
                storage_addr,
                vec![(storage_path, Some(BranchNodeCompact::default()))],
            )
            .expect("storage trie");

        let status = store.initial_state_anchor().expect("status");
        assert_eq!(status.block, Some(anchor));
        assert!(matches!(status.status, InitialStateStatus::InProgress));
        assert_eq!(status.latest_account_trie_key, Some(StoredNibbles::from(account_path)));
        assert_eq!(
            status.latest_storage_trie_key,
            Some(StorageTrieKey::new(storage_addr, StoredNibbles::from(storage_path))),
        );
        assert_eq!(status.latest_hashed_account_key, Some(account_addr));
        assert_eq!(
            status.latest_hashed_storage_key,
            Some(HashedStorageKey::new(storage_addr, slot))
        );
    }

    #[test]
    fn initial_state_commit_sets_v2_earliest_and_latest() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let anchor = BlockNumHash::new(10, B256::from([0xAA; 32]));

        store.set_initial_state_anchor(anchor).expect("set anchor");
        let committed = store.commit_initial_state().expect("commit initial state");

        assert_eq!(committed, anchor);
        assert_eq!(store.get_earliest_block_number().unwrap(), Some((anchor.number, anchor.hash)));
        assert_eq!(store.get_latest_block_number().unwrap(), Some((anchor.number, anchor.hash)));
        assert_no_v1_history(&store);
    }

    #[test]
    fn schema_validation_rejects_legacy_non_empty_store() {
        let dir = TempDir::new().unwrap();
        let env = init_db_for::<_, Tables>(dir.path(), DatabaseArguments::default()).expect("env");
        env.update(|tx| {
            tx.cursor_write::<ProofWindow>()?.upsert(
                ProofWindowKey::LatestBlock,
                &BlockNumberHash::new(1, B256::from([0x01; 32])),
            )?;
            Ok::<(), DatabaseError>(())
        })
        .unwrap()
        .unwrap();
        drop(env);

        let err = MdbxProofsStorage::new(dir.path()).expect_err("legacy schema must fail");
        assert!(matches!(err, BaseProofsStorageError::UnsupportedSchemaVersion { actual: None }));
    }

    #[test]
    fn schema_validation_rejects_wrong_v2_version() {
        let dir = TempDir::new().unwrap();
        let env = init_db_for::<_, Tables>(dir.path(), DatabaseArguments::default()).expect("env");
        env.update(|tx| {
            tx.cursor_write::<V2ProofWindow>()?
                .upsert(ProofWindowKey::SchemaVersion, &BlockNumberHash::new(3, B256::ZERO))?;
            Ok::<(), DatabaseError>(())
        })
        .unwrap()
        .unwrap();
        drop(env);

        let err = MdbxProofsStorage::new(dir.path()).expect_err("wrong schema must fail");
        assert!(matches!(
            err,
            BaseProofsStorageError::UnsupportedSchemaVersion { actual: Some(3) }
        ));
    }

    #[test]
    fn set_initial_state_anchor_rejects_duplicate_anchor() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let first = BlockNumHash::new(10, B256::from([0xAA; 32]));
        let second = BlockNumHash::new(11, B256::from([0xBB; 32]));

        store.set_initial_state_anchor(first).expect("first anchor");
        let err = store.set_initial_state_anchor(second).expect_err("duplicate anchor");
        assert!(matches!(err, BaseProofsStorageError::DatabaseError(_)));
        assert_eq!(store.get_initial_state_anchor().unwrap(), Some(first));
    }

    #[test]
    fn commit_initial_state_without_anchor_returns_no_blocks_found() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        let err = store.commit_initial_state().expect_err("missing anchor");
        assert!(matches!(err, BaseProofsStorageError::NoBlocksFound));
        assert!(store.get_earliest_block_number().unwrap().is_none());
        assert!(store.get_latest_block_number().unwrap().is_none());
    }

    #[test]
    fn fetch_trie_updates_without_block_index_returns_no_change_set() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        let err = store.fetch_trie_updates(42).expect_err("missing changeset");
        assert!(matches!(err, BaseProofsStorageError::NoChangeSetForBlock(42)));
    }

    #[test]
    fn store_trie_updates_writes_v2_current_changeset_history_and_index() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x42; 32]);
        let first = Account { nonce: 1, balance: U256::from(100), ..Default::default() };
        let second = Account { nonce: 2, balance: U256::from(200), ..Default::default() };
        let block_1 = block(1, B256::ZERO);
        let block_2 = block(2, block_1.block.hash);

        store.store_trie_updates(block_1, account_diff(address, Some(first))).expect("block 1");
        store.store_trie_updates(block_2, account_diff(address, Some(second))).expect("block 2");

        let tx = store.env.tx().expect("ro tx");
        let current = tx
            .cursor_read::<V2HashedAccounts>()
            .unwrap()
            .seek_exact(address)
            .unwrap()
            .map(|(_, account)| account);
        assert_eq!(current, Some(second));

        let old = tx
            .cursor_dup_read::<V2HashedAccountChangeSets>()
            .unwrap()
            .seek_by_key_subkey(2, address)
            .unwrap()
            .expect("changeset entry");
        assert_eq!(old.info, Some(first));

        let blocks = tx
            .cursor_read::<V2HashedAccountsHistory>()
            .unwrap()
            .seek_exact(HashedAccountShardedKey::new(address, u64::MAX))
            .unwrap()
            .expect("history entry")
            .1;
        assert!(blocks.contains(1));
        assert!(blocks.contains(2));

        let change_set = tx.cursor_read::<BlockChangeSet>().unwrap().seek_exact(2).unwrap();
        assert!(change_set.expect("block index").1.hashed_account_keys.contains(&address));
        assert_no_v1_history(&store);
    }

    #[test]
    fn historical_account_cursor_uses_first_change_after_requested_block() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x42; 32]);
        let first = Account { nonce: 1, ..Default::default() };
        let second = Account { nonce: 2, ..Default::default() };
        let block_1 = block(1, B256::ZERO);
        let block_2 = block(2, block_1.block.hash);

        store.store_trie_updates(block_1, account_diff(address, Some(first))).expect("block 1");
        store.store_trie_updates(block_2, account_diff(address, Some(second))).expect("block 2");

        let tx = store.env.tx().expect("tx");
        let mut at_1 = MdbxV2AccountCursor::new(&tx, 1).expect("cursor at 1");
        let mut at_2 = MdbxV2AccountCursor::new(&tx, 2).expect("cursor at 2");
        assert_eq!(at_1.seek(address).unwrap().map(|(_, account)| account), Some(first));
        assert_eq!(at_2.seek(address).unwrap().map(|(_, account)| account), Some(second));
    }

    #[test]
    fn historical_storage_cursor_treats_deleted_key_as_absent() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x42; 32]);
        let slot = B256::from([0x24; 32]);
        let block_1 = block(1, B256::ZERO);
        let block_2 = block(2, block_1.block.hash);

        store
            .store_trie_updates(block_1, storage_diff(address, slot, U256::from(10)))
            .expect("block 1");
        store
            .store_trie_updates(block_2, storage_diff(address, slot, U256::ZERO))
            .expect("block 2");

        let tx = store.env.tx().expect("tx");
        let mut at_1 = MdbxV2StorageCursor::new(&tx, 1, address).expect("cursor at 1");
        let mut at_2 = MdbxV2StorageCursor::new(&tx, 2, address).expect("cursor at 2");
        assert_eq!(at_1.seek(slot).unwrap().map(|(_, value)| value), Some(U256::from(10)));
        assert!(at_2.seek(slot).unwrap().is_none());
    }

    #[test]
    fn fetch_trie_updates_filters_exact_seek_for_deleted_account() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let deleted = B256::from([0x10; 32]);
        let next = B256::from([0x20; 32]);
        let block_1 = block(1, B256::ZERO);
        let block_2 = block(2, block_1.block.hash);

        store
            .store_trie_updates(block_1, account_diff(next, Some(Account::default())))
            .expect("seed");
        store.store_trie_updates(block_2, account_diff(deleted, None)).expect("delete");

        let diff = store.fetch_trie_updates(2).expect("fetch");
        assert_eq!(
            diff.sorted_post_state
                .accounts
                .iter()
                .find(|(address, _)| *address == deleted)
                .map(|(_, account)| account),
            Some(&None),
        );
    }

    #[test]
    fn fetch_trie_updates_reconstructs_all_v2_tables() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let addr1 = B256::from([0x11; 32]);
        let addr2 = B256::from([0x22; 32]);
        let slot1 = B256::from([0xA1; 32]);
        let slot2 = B256::from([0xA2; 32]);
        let account_path = Nibbles::from_nibbles_unchecked(vec![0, 1, 2, 3]);
        let storage_path = Nibbles::from_nibbles_unchecked(vec![1, 2, 3, 4]);
        let account_node =
            BranchNodeCompact { root_hash: Some(B256::random()), ..Default::default() };
        let storage_node =
            BranchNodeCompact { root_hash: Some(B256::random()), ..Default::default() };
        let mut trie_updates = TrieUpdates::default();
        trie_updates.account_nodes.insert(account_path, account_node);
        let mut storage_updates = StorageTrieUpdates::default();
        storage_updates.storage_nodes.insert(storage_path, storage_node);
        trie_updates.storage_tries.insert(addr1, storage_updates);
        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(addr1, Some(Account { nonce: 1, ..Default::default() }));
        post_state.accounts.insert(addr2, None);
        let mut storage1 = HashedStorage::default();
        storage1.storage.insert(slot1, U256::from(1234));
        post_state.storages.insert(addr1, storage1);
        let mut storage2 = HashedStorage::default();
        storage2.storage.insert(slot2, U256::from(5678));
        post_state.storages.insert(addr2, storage2);
        let expected = BlockStateDiff {
            sorted_trie_updates: trie_updates.into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        };

        store.store_trie_updates(block(1, B256::ZERO), expected.clone()).expect("store");
        let actual = store.fetch_trie_updates(1).expect("fetch");
        assert_eq!(
            actual.sorted_trie_updates.account_nodes_ref(),
            expected.sorted_trie_updates.account_nodes_ref(),
        );
        assert_eq!(
            actual.sorted_trie_updates.storage_tries_ref(),
            expected.sorted_trie_updates.storage_tries_ref(),
        );
        assert_eq!(actual.sorted_post_state.accounts, expected.sorted_post_state.accounts);
        assert_eq!(actual.sorted_post_state.storages, expected.sorted_post_state.storages);
    }

    #[test]
    fn wipe_records_old_values_and_applies_overlay_in_v2() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x24; 32]);
        let slot = B256::from([0x25; 32]);
        let old_value = U256::from(10);
        let new_value = U256::from(20);
        let block_1 = block(1, B256::ZERO);
        let block_2 = block(2, block_1.block.hash);

        store.store_trie_updates(block_1, storage_diff(address, slot, old_value)).expect("block 1");
        let mut wiped_storage = HashedStorage::new(true);
        wiped_storage.storage.insert(slot, new_value);
        let mut post_state = HashedPostState::default();
        post_state.storages.insert(address, wiped_storage);
        store
            .store_trie_updates(
                block_2,
                BlockStateDiff {
                    sorted_trie_updates: TrieUpdates::default().into_sorted(),
                    sorted_post_state: post_state.into_sorted(),
                },
            )
            .expect("block 2");

        let tx = store.env.tx().expect("ro tx");
        let old = tx
            .cursor_dup_read::<V2HashedStorageChangeSets>()
            .unwrap()
            .seek_by_key_subkey(BlockNumberHashedAddress((2, address)), slot)
            .unwrap()
            .expect("changeset entry");
        assert_eq!(old.value, old_value);
        let latest = tx
            .cursor_dup_read::<V2HashedStorages>()
            .unwrap()
            .seek_by_key_subkey(address, slot)
            .unwrap()
            .expect("current storage");
        assert_eq!(latest.value, new_value);
    }

    #[test]
    fn storage_trie_wipe_records_old_nodes_and_applies_overlay_in_v2() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x24; 32]);
        let old_path = Nibbles::from_nibbles_unchecked([0x01, 0x02]);
        let new_path = Nibbles::from_nibbles_unchecked([0x03, 0x04]);
        let old_node =
            BranchNodeCompact { root_hash: Some(B256::from([0x10; 32])), ..Default::default() };
        let new_node =
            BranchNodeCompact { root_hash: Some(B256::from([0x20; 32])), ..Default::default() };
        let block_1 = block(1, B256::ZERO);
        let block_2 = block(2, block_1.block.hash);
        let mut trie_updates = TrieUpdates::default();
        let mut storage_updates = StorageTrieUpdates::default();
        storage_updates.storage_nodes.insert(old_path, old_node.clone());
        trie_updates.storage_tries.insert(address, storage_updates);
        store
            .store_trie_updates(
                block_1,
                BlockStateDiff {
                    sorted_trie_updates: trie_updates.into_sorted(),
                    sorted_post_state: HashedPostState::default().into_sorted(),
                },
            )
            .expect("block 1");

        let mut wipe = TrieUpdates::default();
        let mut replacement = StorageTrieUpdates::default();
        replacement.set_deleted(true);
        replacement.storage_nodes.insert(new_path, new_node.clone());
        wipe.storage_tries.insert(address, replacement);
        store
            .store_trie_updates(
                block_2,
                BlockStateDiff {
                    sorted_trie_updates: wipe.into_sorted(),
                    sorted_post_state: HashedPostState::default().into_sorted(),
                },
            )
            .expect("block 2");

        let tx = store.env.tx().expect("tx");
        let old = tx
            .cursor_dup_read::<V2StorageTrieChangeSets>()
            .unwrap()
            .seek_by_key_subkey(
                BlockNumberHashedAddress((2, address)),
                StoredNibblesSubKey::from(old_path),
            )
            .unwrap()
            .expect("old node changeset");
        assert_eq!(old.node, Some(old_node));
        let mut current = tx.cursor_dup_read::<V2StoragesTrie>().unwrap();
        let old_subkey = StoredNibblesSubKey::from(old_path);
        assert!(
            current
                .seek_by_key_subkey(address, old_subkey.clone())
                .unwrap()
                .filter(|entry| entry.nibbles == old_subkey)
                .is_none()
        );
        assert_eq!(
            current
                .seek_by_key_subkey(address, StoredNibblesSubKey::from(new_path))
                .unwrap()
                .map(|entry| entry.node),
            Some(new_node),
        );
    }

    #[test]
    fn prune_deletes_v2_history_changesets_and_block_index_without_changing_current() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x42; 32]);
        let first = Account { nonce: 1, ..Default::default() };
        let second = Account { nonce: 2, ..Default::default() };
        let b0 = NumHash::new(0, B256::ZERO);
        let b1 = block(1, b0.hash);
        let b2 = block(2, b1.block.hash);
        store.set_earliest_block_number_hash(b0.number, b0.hash).expect("earliest");
        store.store_trie_updates(b1, account_diff(address, Some(first))).expect("block 1");
        store.store_trie_updates(b2, account_diff(address, Some(second))).expect("block 2");

        let counts = store.prune_earliest_state(b2).expect("prune");
        assert_eq!(counts.hashed_accounts_written_total, 2);
        assert_eq!(store.get_earliest_block_number().unwrap(), Some((2, b2.block.hash)));

        let tx = store.env.tx().expect("tx");
        assert_eq!(
            tx.cursor_read::<V2HashedAccounts>()
                .unwrap()
                .seek_exact(address)
                .unwrap()
                .map(|(_, account)| account),
            Some(second),
        );
        assert!(
            tx.cursor_dup_read::<V2HashedAccountChangeSets>()
                .unwrap()
                .seek_by_key_subkey(1, address)
                .unwrap()
                .is_none()
        );
        assert!(
            tx.cursor_dup_read::<V2HashedAccountChangeSets>()
                .unwrap()
                .seek_by_key_subkey(2, address)
                .unwrap()
                .is_none()
        );
        assert!(tx.cursor_read::<BlockChangeSet>().unwrap().seek_exact(1).unwrap().is_none());
        assert!(tx.cursor_read::<BlockChangeSet>().unwrap().seek_exact(2).unwrap().is_none());
        assert!(
            !MdbxProofsStorage::v2_history_contains::<V2HashedAccountsHistory>(
                &tx,
                HashedAccountShardedKey::new(address, u64::MAX),
                1,
            )
            .unwrap()
        );
        assert!(
            !MdbxProofsStorage::v2_history_contains::<V2HashedAccountsHistory>(
                &tx,
                HashedAccountShardedKey::new(address, u64::MAX),
                2,
            )
            .unwrap()
        );
    }

    #[test]
    fn prune_deletes_all_v2_history_shards() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x33; 32]);
        let slot = B256::from([0x44; 32]);
        let account_path = Nibbles::from_nibbles_unchecked([0x01, 0x02]);
        let storage_path = Nibbles::from_nibbles_unchecked([0x03, 0x04]);
        let mut trie_updates = TrieUpdates::default();
        trie_updates.account_nodes.insert(account_path, BranchNodeCompact::default());
        let mut storage_updates = StorageTrieUpdates::default();
        storage_updates.storage_nodes.insert(storage_path, BranchNodeCompact::default());
        trie_updates.storage_tries.insert(address, storage_updates);
        let mut storage = HashedStorage::default();
        storage.storage.insert(slot, U256::from(7));
        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(address, Some(Account { nonce: 1, ..Default::default() }));
        post_state.storages.insert(address, storage);
        let b0 = NumHash::new(0, B256::ZERO);
        let b1 = block(1, b0.hash);

        store.set_earliest_block_number_hash(b0.number, b0.hash).expect("earliest");
        store
            .store_trie_updates(
                b1,
                BlockStateDiff {
                    sorted_trie_updates: trie_updates.into_sorted(),
                    sorted_post_state: post_state.into_sorted(),
                },
            )
            .expect("block 1");
        store.prune_earliest_state(b1).expect("prune");

        let tx = store.env.tx().expect("tx");
        assert!(
            !MdbxProofsStorage::v2_history_contains::<V2AccountsTrieHistory>(
                &tx,
                AccountTrieShardedKey::new(StoredNibbles::from(account_path), u64::MAX),
                1,
            )
            .unwrap()
        );
        assert!(
            !MdbxProofsStorage::v2_history_contains::<V2StoragesTrieHistory>(
                &tx,
                StorageTrieShardedKey::new(address, StoredNibbles::from(storage_path), u64::MAX),
                1,
            )
            .unwrap()
        );
        assert!(
            !MdbxProofsStorage::v2_history_contains::<V2HashedAccountsHistory>(
                &tx,
                HashedAccountShardedKey::new(address, u64::MAX),
                1,
            )
            .unwrap()
        );
        assert!(
            !MdbxProofsStorage::v2_history_contains::<V2HashedStoragesHistory>(
                &tx,
                HashedStorageShardedKey::new(address, slot, u64::MAX),
                1,
            )
            .unwrap()
        );
    }

    #[test]
    fn unwind_restores_v2_current_and_deletes_newer_history() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x42; 32]);
        let first = Account { nonce: 1, ..Default::default() };
        let second = Account { nonce: 2, ..Default::default() };
        let third = Account { nonce: 3, ..Default::default() };
        let b0 = NumHash::new(0, B256::ZERO);
        let b1 = block(1, b0.hash);
        let b2 = block(2, b1.block.hash);
        let b3 = block(3, b2.block.hash);
        store.set_earliest_block_number_hash(b0.number, b0.hash).expect("earliest");
        store.store_trie_updates(b1, account_diff(address, Some(first))).expect("block 1");
        store.store_trie_updates(b2, account_diff(address, Some(second))).expect("block 2");
        store.store_trie_updates(b3, account_diff(address, Some(third))).expect("block 3");

        store.unwind_history(b2).expect("unwind");

        let tx = store.env.tx().expect("tx");
        let current = tx
            .cursor_read::<V2HashedAccounts>()
            .unwrap()
            .seek_exact(address)
            .unwrap()
            .map(|(_, account)| account);
        assert_eq!(current, Some(first));
        assert!(tx.cursor_read::<BlockChangeSet>().unwrap().seek_exact(1).unwrap().is_some());
        assert!(tx.cursor_read::<BlockChangeSet>().unwrap().seek_exact(2).unwrap().is_none());
        assert!(tx.cursor_read::<BlockChangeSet>().unwrap().seek_exact(3).unwrap().is_none());
        assert_eq!(store.get_latest_block_number().unwrap(), Some((1, b2.parent)));
    }

    #[test]
    fn unwind_restores_storage_current_and_removes_new_slots() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x42; 32]);
        let slot = B256::from([0x24; 32]);
        let new_slot = B256::from([0x25; 32]);
        let b0 = NumHash::new(0, B256::ZERO);
        let b1 = block(1, b0.hash);
        let b2 = block(2, b1.block.hash);
        let b3 = block(3, b2.block.hash);
        store.set_earliest_block_number_hash(b0.number, b0.hash).expect("earliest");
        store.store_trie_updates(b1, storage_diff(address, slot, U256::from(10))).expect("b1");
        store.store_trie_updates(b2, storage_diff(address, slot, U256::from(20))).expect("b2");
        store.store_trie_updates(b3, storage_diff(address, new_slot, U256::from(30))).expect("b3");

        store.unwind_history(b2).expect("unwind");

        let tx = store.env.tx().expect("tx");
        let mut cursor = tx.cursor_dup_read::<V2HashedStorages>().unwrap();
        assert_eq!(
            cursor.seek_by_key_subkey(address, slot).unwrap().map(|entry| entry.value),
            Some(U256::from(10)),
        );
        assert!(cursor.seek_by_key_subkey(address, new_slot).unwrap().is_none());
    }

    #[test]
    fn unwind_restores_storage_trie_nodes_after_wipe() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0x55; 32]);
        let path = Nibbles::from_nibbles_unchecked([0x01, 0x02]);
        let node =
            BranchNodeCompact { root_hash: Some(B256::from([0x10; 32])), ..Default::default() };
        let b0 = NumHash::new(0, B256::ZERO);
        let b1 = block(1, b0.hash);
        let b2 = block(2, b1.block.hash);
        let mut trie_updates = TrieUpdates::default();
        let mut storage_updates = StorageTrieUpdates::default();
        storage_updates.storage_nodes.insert(path, node.clone());
        trie_updates.storage_tries.insert(address, storage_updates);

        store.set_earliest_block_number_hash(b0.number, b0.hash).expect("earliest");
        store
            .store_trie_updates(
                b1,
                BlockStateDiff {
                    sorted_trie_updates: trie_updates.into_sorted(),
                    sorted_post_state: HashedPostState::default().into_sorted(),
                },
            )
            .expect("b1");
        let mut wipe = TrieUpdates::default();
        let mut deleted = StorageTrieUpdates::default();
        deleted.set_deleted(true);
        wipe.storage_tries.insert(address, deleted);
        store
            .store_trie_updates(
                b2,
                BlockStateDiff {
                    sorted_trie_updates: wipe.into_sorted(),
                    sorted_post_state: HashedPostState::default().into_sorted(),
                },
            )
            .expect("b2");

        store.unwind_history(b2).expect("unwind");

        let tx = store.env.tx().expect("tx");
        assert_eq!(
            tx.cursor_dup_read::<V2StoragesTrie>()
                .unwrap()
                .seek_by_key_subkey(address, StoredNibblesSubKey::from(path))
                .unwrap()
                .map(|entry| entry.node),
            Some(node),
        );
    }

    #[test]
    fn replace_updates_restores_common_v2_state_and_appends_new_chain() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let address = B256::from([0xAB; 32]);
        let b1 = block(1, B256::ZERO);
        let b2 = block(2, b1.block.hash);
        let b3 = block(3, b2.block.hash);
        store
            .store_trie_updates(
                b1,
                account_diff(address, Some(Account { nonce: 10, ..Default::default() })),
            )
            .expect("b1");
        store
            .store_trie_updates(
                b2,
                account_diff(address, Some(Account { nonce: 20, ..Default::default() })),
            )
            .expect("b2");
        store
            .store_trie_updates(
                b3,
                account_diff(address, Some(Account { nonce: 30, ..Default::default() })),
            )
            .expect("b3");

        let b3p = BlockWithParent::new(b2.block.hash, NumHash::new(3, B256::from([0x33; 32])));
        let b4p = BlockWithParent::new(b3p.block.hash, NumHash::new(4, B256::from([0x44; 32])));
        store
            .replace_updates(
                BlockNumHash::new(2, b2.block.hash),
                vec![
                    (
                        b3p,
                        account_diff(address, Some(Account { nonce: 300, ..Default::default() })),
                    ),
                    (
                        b4p,
                        account_diff(address, Some(Account { nonce: 400, ..Default::default() })),
                    ),
                ],
            )
            .expect("replace");

        let tx = store.env.tx().expect("tx");
        let current = tx
            .cursor_read::<V2HashedAccounts>()
            .unwrap()
            .seek_exact(address)
            .unwrap()
            .map(|(_, account)| account.nonce);
        assert_eq!(current, Some(400));
        assert_eq!(store.get_latest_block_number().unwrap(), Some((4, b4p.block.hash)));
        let mut seen = BTreeSet::new();
        let mut cursor = tx.cursor_read::<BlockChangeSet>().unwrap();
        let mut walker = cursor.walk(Some(1)).unwrap();
        while let Some(Ok((number, _))) = walker.next() {
            seen.insert(number);
        }
        assert_eq!(seen, [1, 2, 3, 4].into_iter().collect::<BTreeSet<_>>());
    }

    #[test]
    fn out_of_order_updates_are_rejected() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let existing = block(1, B256::ZERO);
        store.set_earliest_block_number(existing.block.number, existing.block.hash).expect("set");
        let bad = BlockWithParent::new(B256::from([0xFF; 32]), NumHash::new(2, B256::ZERO));
        let res = store.store_trie_updates(bad, BlockStateDiff::default());
        assert!(matches!(res, Err(BaseProofsStorageError::OutOfOrder { .. })));
        assert_eq!(store.get_latest_block_number().unwrap(), Some((1, existing.block.hash)));
    }

    #[test]
    fn empty_diff_records_block_index_and_latest() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let b1 = block(1, B256::ZERO);
        store.store_trie_updates(b1, BlockStateDiff::default()).expect("store");
        let tx = store.env.tx().expect("tx");
        assert!(tx.cursor_read::<BlockChangeSet>().unwrap().seek_exact(1).unwrap().is_some());
        assert_eq!(store.get_latest_block_number().unwrap(), Some((1, b1.block.hash)));
    }

    #[test]
    fn unwind_guards_earliest_and_beyond_latest() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let b1 = block(1, B256::ZERO);
        let b2 = block(2, b1.block.hash);
        store.set_earliest_block_number_hash(b1.block.number, b1.block.hash).expect("earliest");
        store
            .store_trie_updates(b2, account_diff(B256::random(), Some(Account::default())))
            .expect("b2");

        let err = store.unwind_history(b1).expect_err("cannot unwind to earliest");
        assert!(matches!(err, BaseProofsStorageError::UnwindBeyondEarliest { .. }));
        let b4 = block(4, B256::random());
        store.unwind_history(b4).expect("beyond latest no-op");
        assert_eq!(store.get_latest_block_number().unwrap(), Some((2, b2.block.hash)));
    }
}
