//! Initialization job for proofs storage. Handles storing the existing state into the proofs
//! storage.

use std::{collections::HashMap, time::Instant};

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256};
use derive_more::Constructor;
use reth_db::{
    DatabaseError,
    cursor::{DbCursorRO, DbDupCursorRO},
    tables,
    transaction::DbTx,
};
use reth_primitives_traits::{Account, StorageEntry};
use reth_trie_common::{
    BranchNodeCompact, Nibbles, StorageTrieEntry, StoredNibbles, StoredNibblesSubKey,
};
use tracing::{debug, info};

use crate::{
    BaseProofsStorageError, BaseProofsStore,
    api::{BaseProofsInitialStateStore, InitialStateAnchor, InitialStateStatus},
    db::{HashedStorageKey, StorageTrieKey},
};

/// Batch size threshold for storing entries during initialization
const INITIALIZE_STORAGE_THRESHOLD: usize = 100000;

/// Threshold for logging progress during initialization
const INITIALIZE_LOG_THRESHOLD: usize = 100000;

/// Initialization job for external storage.
#[derive(Debug, Constructor)]
pub struct InitializationJob<Tx: DbTx, S: BaseProofsStore + Send> {
    storage: S,
    tx: Tx,
}

/// Macro to generate simple cursor iterators for tables
macro_rules! define_simple_cursor_iter {
    ($iter_name:ident, $table:ty, $key_type:ty, $value_type:ty) => {
        struct $iter_name<C>(C);

        impl<C> $iter_name<C> {
            const fn new(cursor: C) -> Self {
                Self(cursor)
            }
        }

        impl<C: DbCursorRO<$table>> Iterator for $iter_name<C> {
            type Item = Result<($key_type, $value_type), DatabaseError>;

            fn next(&mut self) -> Option<Self::Item> {
                self.0.next().transpose()
            }
        }
    };
}

/// Macro to generate duplicate cursor iterators for tables with custom logic
macro_rules! define_dup_cursor_iter {
    ($iter_name:ident, $table:ty, $key_type:ty, $value_type:ty) => {
        struct $iter_name<C>(C);

        impl<C> $iter_name<C> {
            const fn new(cursor: C) -> Self {
                Self(cursor)
            }
        }

        impl<C: DbDupCursorRO<$table> + DbCursorRO<$table>> Iterator for $iter_name<C> {
            type Item = Result<($key_type, $value_type), DatabaseError>;

            fn next(&mut self) -> Option<Self::Item> {
                // First try to get the next duplicate value
                if let Some(res) = self.0.next_dup().transpose() {
                    return Some(res);
                }

                // If no more duplicates, find the next key with values
                let Some(Ok((next_key, _))) = self.0.next_no_dup().transpose() else {
                    // If no more entries, return None
                    return None;
                };

                // If found, seek to the first duplicate for this key
                return self.0.seek(next_key).transpose();
            }
        }
    };
}

// Generate iterators for all 4 table types
define_simple_cursor_iter!(HashedAccountsInit, tables::HashedAccounts, B256, Account);
define_dup_cursor_iter!(HashedStoragesInit, tables::HashedStorages, B256, StorageEntry);
define_simple_cursor_iter!(
    AccountsTrieInit,
    tables::AccountsTrie,
    StoredNibbles,
    BranchNodeCompact
);
define_dup_cursor_iter!(StoragesTrieInit, tables::StoragesTrie, B256, StorageTrieEntry);

/// Trait to estimate the progress of a initialization job based on the key.
trait CompletionEstimatable {
    // Returns a progress estimate as a percentage (0.0 to 1.0)
    fn estimate_progress(&self) -> f64;
}

impl CompletionEstimatable for B256 {
    fn estimate_progress(&self) -> f64 {
        // use the first 3 bytes as a progress estimate
        let progress = self.0[..3].to_vec();
        let mut val: u64 = 0;
        for nibble in &progress {
            val = (val << 8) | *nibble as u64;
        }
        val as f64 / (256u64.pow(3)) as f64
    }
}

impl CompletionEstimatable for StoredNibbles {
    fn estimate_progress(&self) -> f64 {
        // use the first 6 nibbles as a progress estimate
        let progress_nibbles =
            if self.0.is_empty() { Nibbles::new() } else { self.0.slice(0..(self.0.len().min(6))) };
        let mut val: u64 = 0;
        for nibble in progress_nibbles.iter() {
            val = (val << 4) | nibble as u64;
        }
        val as f64 / (16u64.pow(progress_nibbles.len() as u32)) as f64
    }
}

impl<Tx: DbTx + Sync, S: BaseProofsStore + BaseProofsInitialStateStore + Send>
    InitializationJob<Tx, S>
{
    /// Initialize a table from a source iterator to a storage function. Handles batching and
    /// logging.
    fn initialize<
        I: Iterator<Item = Result<(Key, Value), DatabaseError>> + InitTable<Key = Key, Value = Value>,
        Key: CompletionEstimatable + 'static,
        Value: 'static,
    >(
        &self,
        name: &str,
        source: I,
        storage_threshold: usize,
        log_threshold: usize,
    ) -> Result<u64, BaseProofsStorageError> {
        info!(name = %name, "Starting initialization");
        let start_time = Instant::now();

        let mut source = source.peekable();
        let Some(first_entry) = source.peek() else {
            debug!(target: "reth::cli", "No entries to store for table");
            return Ok(0);
        };
        let initial_progress = match first_entry {
            Ok(i) => i.0.estimate_progress(),
            Err(e) => Err(e.clone())?,
        };

        let storage = &self.storage;
        let source_size_hint = source.size_hint().0;
        let mut batch = Vec::with_capacity(source_size_hint.min(storage_threshold));
        let mut total_entries: usize = 0;

        for entry in source {
            batch.push(entry?);
            total_entries += 1;

            if total_entries.is_multiple_of(log_threshold) {
                let progress = batch.last().expect("non-empty batch").0.estimate_progress();
                let elapsed = start_time.elapsed();
                let elapsed_secs = elapsed.as_secs_f64();

                let progress_per_second = if elapsed_secs.is_normal() {
                    (progress - initial_progress) / elapsed_secs
                } else {
                    0.0
                };
                let estimated_total_time = if progress_per_second.is_normal() {
                    (1.0 - progress) / progress_per_second
                } else {
                    0.0
                };
                let progress_pct = progress * 100.0;
                info!(
                    name = %name,
                    total_entries,
                    progress_pct = format_args!("{:.2}", progress_pct),
                    estimated_total_time = format_args!("{:.1}", estimated_total_time),
                    "Processed entries"
                );
            }

            if batch.len() >= storage_threshold {
                info!(name = %name, total_entries = total_entries, "Storing entries");
                I::store_entries(storage, batch)?;
                batch = Vec::with_capacity(
                    (source_size_hint.saturating_sub(total_entries)).min(storage_threshold),
                );
            }
        }

        if !batch.is_empty() {
            info!(name = %name, "Storing final entries");
            I::store_entries(storage, batch)?;
        }

        info!(name = %name, total_entries = total_entries, "initialization complete");
        Ok(total_entries as u64)
    }

    /// Initialize hashed accounts data
    fn initialize_hashed_accounts(
        &self,
        start_key: Option<B256>,
    ) -> Result<(), BaseProofsStorageError> {
        let mut start_cursor = self.tx.cursor_read::<tables::HashedAccounts>()?;

        if let Some(latest) = start_key {
            start_cursor
                .seek(latest)?
                .filter(|(k, _)| *k == latest)
                .ok_or(BaseProofsStorageError::InitializeStorageInconsistentState)?;
        }

        let source = HashedAccountsInit::new(start_cursor);
        self.initialize(
            "hashed accounts",
            source,
            INITIALIZE_STORAGE_THRESHOLD,
            INITIALIZE_LOG_THRESHOLD,
        )?;

        Ok(())
    }

    /// Initialize hashed storage data
    fn initialize_hashed_storages(
        &self,
        start_key: Option<HashedStorageKey>,
    ) -> Result<(), BaseProofsStorageError> {
        let mut start_cursor = self.tx.cursor_dup_read::<tables::HashedStorages>()?;

        if let Some(latest) = start_key {
            start_cursor
                .seek_by_key_subkey(latest.hashed_address, latest.hashed_storage_key)?
                .filter(|v| v.key == latest.hashed_storage_key)
                .ok_or(BaseProofsStorageError::InitializeStorageInconsistentState)?;
        }

        let source = HashedStoragesInit::new(start_cursor);
        self.initialize(
            "hashed storage",
            source,
            INITIALIZE_STORAGE_THRESHOLD,
            INITIALIZE_LOG_THRESHOLD,
        )?;

        Ok(())
    }

    /// Initialize accounts trie data
    fn initialize_accounts_trie(
        &self,
        start_key: Option<StoredNibbles>,
    ) -> Result<(), BaseProofsStorageError> {
        let mut start_cursor = self.tx.cursor_read::<tables::AccountsTrie>()?;

        if let Some(latest_key) = start_key {
            start_cursor
                .seek(latest_key.clone())?
                .filter(|(k, _)| *k == latest_key)
                .ok_or(BaseProofsStorageError::InitializeStorageInconsistentState)?;
        }

        let source = AccountsTrieInit::new(start_cursor);
        self.initialize(
            "accounts trie",
            source,
            INITIALIZE_STORAGE_THRESHOLD,
            INITIALIZE_LOG_THRESHOLD,
        )?;

        Ok(())
    }

    /// Initialize storage trie data
    fn initialize_storages_trie(
        &self,
        start_key: Option<StorageTrieKey>,
    ) -> Result<(), BaseProofsStorageError> {
        let mut start_cursor = self.tx.cursor_dup_read::<tables::StoragesTrie>()?;

        if let Some(latest_key) = start_key {
            start_cursor
                .seek_by_key_subkey(
                    latest_key.hashed_address,
                    StoredNibblesSubKey::from(latest_key.path.0),
                )?
                .filter(|v| v.nibbles.0 == latest_key.path.0)
                .ok_or(BaseProofsStorageError::InitializeStorageInconsistentState)?;
        }

        let source = StoragesTrieInit::new(start_cursor);
        self.initialize(
            "storage trie",
            source,
            INITIALIZE_STORAGE_THRESHOLD,
            INITIALIZE_LOG_THRESHOLD,
        )?;

        Ok(())
    }

    /// Run complete initialization of all preimage data
    fn initialize_trie(&self, anchor: InitialStateAnchor) -> Result<(), BaseProofsStorageError> {
        self.initialize_hashed_accounts(anchor.latest_hashed_account_key)?;
        self.initialize_hashed_storages(anchor.latest_hashed_storage_key)?;
        self.initialize_storages_trie(anchor.latest_storage_trie_key)?;
        self.initialize_accounts_trie(anchor.latest_account_trie_key)?;
        Ok(())
    }

    fn validate_anchor_block(
        &self,
        anchor: &InitialStateAnchor,
        best_number: u64,
        best_hash: B256,
    ) -> Result<(), BaseProofsStorageError> {
        let block =
            anchor.block.ok_or(BaseProofsStorageError::InitializeStorageInconsistentState)?;

        if block.number != best_number || block.hash != best_hash {
            return Err(BaseProofsStorageError::InitializeStorageInconsistentState);
        }

        Ok(())
    }

    /// Run the initialization job.
    pub fn run(&self, best_number: u64, best_hash: B256) -> Result<(), BaseProofsStorageError> {
        let anchor = self.storage.initial_state_anchor()?;

        match anchor.status {
            InitialStateStatus::Completed => return Ok(()),
            InitialStateStatus::NotStarted => {
                self.storage.set_initial_state_anchor(BlockNumHash::new(best_number, best_hash))?;
            }
            InitialStateStatus::InProgress => {
                self.validate_anchor_block(&anchor, best_number, best_hash)?;
            }
        }

        self.initialize_trie(anchor)?;
        self.storage.commit_initial_state()?;

        Ok(())
    }
}

/// Handles storing entries for a particular KV-pair type.
trait InitTable {
    /// Key of target table.
    type Key: CompletionEstimatable + 'static;
    /// Value of target table.
    type Value: 'static;

    /// Writes given entries to given storage.
    fn store_entries(
        store: &impl BaseProofsInitialStateStore,
        entries: impl IntoIterator<Item = (Self::Key, Self::Value)>,
    ) -> Result<(), BaseProofsStorageError>;
}

impl<C> InitTable for HashedAccountsInit<C> {
    type Key = B256;
    type Value = Account;

    /// Save mapping of hashed addresses to accounts to storage.
    fn store_entries(
        store: &impl BaseProofsInitialStateStore,
        entries: impl IntoIterator<Item = (Self::Key, Self::Value)>,
    ) -> Result<(), BaseProofsStorageError> {
        store.store_hashed_accounts(
            entries.into_iter().map(|(address, account)| (address, Some(account))).collect(),
        )?;
        Ok(())
    }
}

impl<C> InitTable for HashedStoragesInit<C> {
    type Key = B256;
    type Value = StorageEntry;

    /// Save mapping of hashed addresses to storage entries to storage.
    fn store_entries(
        store: &impl BaseProofsInitialStateStore,
        entries: impl IntoIterator<Item = (Self::Key, Self::Value)>,
    ) -> Result<(), BaseProofsStorageError> {
        let entries_iter = entries.into_iter();
        let mut by_address: HashMap<B256, Vec<(B256, U256)>> =
            HashMap::with_capacity(entries_iter.size_hint().0);

        // Group entries by hashed address
        for (address, entry) in entries_iter {
            by_address.entry(address).or_default().push((entry.key, entry.value));
        }
        // Store each address's storage entries
        for (address, storages) in by_address {
            store.store_hashed_storages(address, storages)?;
        }

        Ok(())
    }
}

impl<C> InitTable for AccountsTrieInit<C> {
    type Key = StoredNibbles;
    type Value = BranchNodeCompact;

    /// Save mapping of account trie paths to branch nodes to storage.
    fn store_entries(
        store: &impl BaseProofsInitialStateStore,
        entries: impl IntoIterator<Item = (Self::Key, Self::Value)>,
    ) -> Result<(), BaseProofsStorageError> {
        store.store_account_branches(
            entries.into_iter().map(|(path, branch)| (path.0, Some(branch))).collect(),
        )?;

        Ok(())
    }
}

impl<C> InitTable for StoragesTrieInit<C> {
    type Key = B256;
    type Value = StorageTrieEntry;

    /// Save mapping of hashed addresses to storage trie entries to storage.
    fn store_entries(
        store: &impl BaseProofsInitialStateStore,
        entries: impl IntoIterator<Item = (Self::Key, Self::Value)>,
    ) -> Result<(), BaseProofsStorageError> {
        let entries_iter = entries.into_iter();
        let mut by_address: HashMap<B256, Vec<(Nibbles, Option<BranchNodeCompact>)>> =
            HashMap::with_capacity(entries_iter.size_hint().0);

        // Group entries by hashed address
        for (hashed_address, storage_entry) in entries_iter {
            by_address
                .entry(hashed_address)
                .or_default()
                .push((storage_entry.nibbles.0, Some(storage_entry.node)));
        }
        // Store each address's storage trie branches
        for (address, branches) in by_address {
            store.store_storage_branches(address, branches)?;
        }

        Ok(())
    }
}
