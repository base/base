//! Writing hashed state and trie node changes into the datadir's tables, as reth's
//! `write_hashed_state` and `write_trie_updates` do.

use base_tiger_store::{Db, Dbi, Rw, StorageEntry, Table, TrieCodec, Txn};

use crate::{HashedState, TrieError, TrieUpdates};

/// Deletes the duplicate of `key` whose value starts with `prefix`, if any.
fn delete_dup(txn: &mut Txn<Rw>, dbi: Dbi, key: &[u8], prefix: &[u8]) -> Result<(), TrieError> {
    let mut cursor = txn.cursor_rw(dbi)?;
    let found = cursor.seek_dup(key, prefix)?.is_some_and(|(_, value)| value.starts_with(prefix));
    if found {
        cursor.delete_current()?;
    }
    Ok(())
}

/// Hashed state and trie table writes.
#[derive(Debug)]
pub struct StateWriter;

impl StateWriter {
    /// Writes accounts (deleting `None`s) and storage (wiping first, deleting zero values) to
    /// `HashedAccounts` and `HashedStorages`.
    pub fn write_hashed_state(txn: &mut Txn<Rw>, db: &Db, state: &HashedState) -> Result<(), TrieError> {
        let accounts = db.dbi(Table::HashedAccounts);
        for (key, account) in &state.accounts {
            match account {
                Some(account) => txn.put(accounts, key.as_slice(), &account.to_bytes())?,
                None => {
                    txn.delete(accounts, key.as_slice(), None)?;
                }
            }
        }
        let storages = db.dbi(Table::HashedStorages);
        for (key, storage) in &state.storages {
            if storage.wiped {
                txn.delete(storages, key.as_slice(), None)?;
            }
            for (slot, value) in &storage.slots {
                if !storage.wiped {
                    delete_dup(txn, storages, key.as_slice(), slot.as_slice())?;
                }
                if !value.is_zero() {
                    txn.put(storages, key.as_slice(), &StorageEntry { key: *slot, value: *value }.to_bytes())?;
                }
            }
        }
        Ok(())
    }

    /// Writes trie node changes to `AccountsTrie` and `StoragesTrie`.
    pub fn write_trie_updates(txn: &mut Txn<Rw>, db: &Db, updates: &TrieUpdates) -> Result<(), TrieError> {
        let accounts = db.dbi(Table::AccountsTrie);
        for (path, node) in &updates.account_nodes {
            let key = TrieCodec::packed_key(path);
            match node {
                Some(node) => {
                    let mut value = Vec::with_capacity(6 + 32 * 17);
                    TrieCodec::encode_node(node, &mut value);
                    txn.put(accounts, &key, &value)?;
                }
                None => {
                    txn.delete(accounts, &key, None)?;
                }
            }
        }
        let storages = db.dbi(Table::StoragesTrie);
        for (address, trie) in &updates.storage_tries {
            if trie.wiped {
                txn.delete(storages, address.as_slice(), None)?;
            }
            for (path, node) in &trie.nodes {
                let subkey = TrieCodec::packed_key(path);
                delete_dup(txn, storages, address.as_slice(), &subkey)?;
                if let Some(node) = node {
                    txn.put(storages, address.as_slice(), &TrieCodec::storage_entry(path, node))?;
                }
            }
        }
        Ok(())
    }
}
