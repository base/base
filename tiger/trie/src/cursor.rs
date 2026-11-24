//! Cursors over the datadir's trie and hashed state tables, and an in-memory overlay of hashed
//! state changes on top of them.

use alloy_primitives::{B256, U256};
use alloy_trie::{BranchNodeCompact, Nibbles};
use base_tiger_store::{Account, Cursor, Db, StorageEntry, Table, TrieCodec, Txn};

use crate::{HashedCursor, TrieCursor, TrieError};

/// The account trie table (`AccountsTrie`, packed keys).
#[derive(Debug)]
pub struct AccountTrieCursor<'t, K> {
    /// The MDBX cursor.
    pub cursor: Cursor<'t, K>,
    /// The last path returned.
    pub current: Option<Nibbles>,
}

impl<'t, K> AccountTrieCursor<'t, K> {
    /// Opens the cursor.
    pub fn new(txn: &'t Txn<K>, db: &Db) -> Result<Self, TrieError> {
        Ok(Self { cursor: txn.cursor(db.dbi(Table::AccountsTrie))?, current: None })
    }

    fn decode(&mut self, entry: Option<(&[u8], &[u8])>) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let Some((key, value)) = entry else {
            self.current = None;
            return Ok(None);
        };
        let path = TrieCodec::unpack_key(key)?;
        let node = TrieCodec::decode_node(value)?;
        self.current = Some(path);
        Ok(Some((path, node)))
    }
}

impl<K> TrieCursor for AccountTrieCursor<'_, K> {
    fn seek_exact(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let entry = self.cursor.seek_exact(&TrieCodec::packed_key(&key))?.map(|(k, v)| (k.to_vec(), v.to_vec()));
        self.decode(entry.as_ref().map(|(k, v)| (k.as_slice(), v.as_slice())))
    }

    fn seek(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let entry = self.cursor.seek(&TrieCodec::packed_key(&key))?.map(|(k, v)| (k.to_vec(), v.to_vec()));
        self.decode(entry.as_ref().map(|(k, v)| (k.as_slice(), v.as_slice())))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let entry = self.cursor.next()?.map(|(k, v)| (k.to_vec(), v.to_vec()));
        self.decode(entry.as_ref().map(|(k, v)| (k.as_slice(), v.as_slice())))
    }

    fn current(&mut self) -> Result<Option<Nibbles>, TrieError> {
        Ok(self.current)
    }
}

/// One account's storage trie in `StoragesTrie` (`DUPSORT`: hashed address, then packed
/// sub-key and node). With `empty`, it holds no nodes (a wiped trie).
#[derive(Debug)]
pub struct StorageTrieCursor<'t, K> {
    /// The MDBX cursor.
    pub cursor: Cursor<'t, K>,
    /// The account.
    pub address: B256,
    /// Treat the trie as empty.
    pub empty: bool,
    /// The last path returned.
    pub current: Option<Nibbles>,
}

impl<'t, K> StorageTrieCursor<'t, K> {
    /// Opens the cursor.
    pub fn new(txn: &'t Txn<K>, db: &Db, address: B256, empty: bool) -> Result<Self, TrieError> {
        Ok(Self { cursor: txn.cursor(db.dbi(Table::StoragesTrie))?, address, empty, current: None })
    }

    fn decode(&mut self, value: Option<Vec<u8>>) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let Some(value) = value else {
            self.current = None;
            return Ok(None);
        };
        let (path, node) = TrieCodec::decode_storage_entry(&value)?;
        self.current = Some(path);
        Ok(Some((path, node)))
    }
}

impl<K> TrieCursor for StorageTrieCursor<'_, K> {
    fn seek_exact(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        match self.seek(key)? {
            Some((path, node)) if path == key => Ok(Some((path, node))),
            _ => {
                self.current = None;
                Ok(None)
            }
        }
    }

    fn seek(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        if self.empty {
            return Ok(None);
        }
        let value = self.cursor.seek_dup(self.address.as_slice(), &TrieCodec::packed_key(&key))?.map(|(_, v)| v.to_vec());
        self.decode(value)
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        if self.empty {
            return Ok(None);
        }
        let value = self.cursor.next_dup()?.map(|(_, v)| v.to_vec());
        self.decode(value)
    }

    fn current(&mut self) -> Result<Option<Nibbles>, TrieError> {
        Ok(self.current)
    }
}

/// Hashed accounts (`HashedAccounts`).
#[derive(Debug)]
pub struct HashedAccountCursor<'t, K> {
    /// The MDBX cursor.
    pub cursor: Cursor<'t, K>,
}

impl<'t, K> HashedAccountCursor<'t, K> {
    /// Opens the cursor.
    pub fn new(txn: &'t Txn<K>, db: &Db) -> Result<Self, TrieError> {
        Ok(Self { cursor: txn.cursor(db.dbi(Table::HashedAccounts))? })
    }
}

fn account_entry(entry: Option<(&[u8], &[u8])>) -> Result<Option<(B256, Account)>, TrieError> {
    let Some((key, value)) = entry else { return Ok(None) };
    if key.len() != 32 {
        return Err(TrieError::Corrupt("hashed account key length".into()));
    }
    Ok(Some((B256::from_slice(key), Account::from_bytes(value)?)))
}

impl<K> HashedCursor for HashedAccountCursor<'_, K> {
    type Value = Account;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Account)>, TrieError> {
        account_entry(self.cursor.seek(key.as_slice())?)
    }

    fn next(&mut self) -> Result<Option<(B256, Account)>, TrieError> {
        account_entry(self.cursor.next()?)
    }
}

/// One account's hashed storage (`HashedStorages`, `DUPSORT`: hashed address, then hashed slot
/// and compact value).
#[derive(Debug)]
pub struct HashedStorageCursor<'t, K> {
    /// The MDBX cursor.
    pub cursor: Cursor<'t, K>,
    /// The account.
    pub address: B256,
}

impl<'t, K> HashedStorageCursor<'t, K> {
    /// Opens the cursor.
    pub fn new(txn: &'t Txn<K>, db: &Db, address: B256) -> Result<Self, TrieError> {
        Ok(Self { cursor: txn.cursor(db.dbi(Table::HashedStorages))?, address })
    }
}

fn storage_entry(entry: Option<(&[u8], &[u8])>) -> Result<Option<(B256, U256)>, TrieError> {
    let Some((_, value)) = entry else { return Ok(None) };
    let entry = StorageEntry::from_bytes(value)?;
    Ok(Some((entry.key, entry.value)))
}

impl<K> HashedCursor for HashedStorageCursor<'_, K> {
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, U256)>, TrieError> {
        storage_entry(self.cursor.seek_dup(self.address.as_slice(), key.as_slice())?)
    }

    fn next(&mut self) -> Result<Option<(B256, U256)>, TrieError> {
        storage_entry(self.cursor.next_dup()?)
    }
}

/// Changes over a base cursor: overlay entries (`None` deletes) replace base entries with the
/// same key; with `wiped`, base entries are ignored.
#[derive(Debug)]
pub struct OverlayCursor<C: HashedCursor> {
    /// The base cursor.
    pub base: C,
    /// Ignore the base.
    pub wiped: bool,
    /// Sorted changes.
    pub overlay: Vec<(B256, Option<C::Value>)>,
    /// Next overlay index.
    pub index: usize,
    /// The next base entry.
    pub base_next: Option<(B256, C::Value)>,
}

impl<C: HashedCursor> OverlayCursor<C> {
    /// An overlay of sorted `overlay` changes over `base`.
    pub const fn new(base: C, wiped: bool, overlay: Vec<(B256, Option<C::Value>)>) -> Self {
        Self { base, wiped, overlay, index: 0, base_next: None }
    }

    fn merged(&mut self) -> Result<Option<(B256, C::Value)>, TrieError> {
        loop {
            let overlay_key = self.overlay.get(self.index).map(|(k, _)| *k);
            let base_key = self.base_next.as_ref().map(|(k, _)| *k);
            match (overlay_key, base_key) {
                (None, None) => return Ok(None),
                (Some(o), b) if b.is_none_or(|b| o <= b) => {
                    let value = self.overlay[self.index].1.clone();
                    self.index += 1;
                    if b == Some(o) {
                        self.base_next = self.base.next()?;
                    }
                    if let Some(value) = value {
                        return Ok(Some((o, value)));
                    }
                }
                _ => {
                    let entry = self.base_next.take();
                    self.base_next = self.base.next()?;
                    return Ok(entry);
                }
            }
        }
    }
}

impl<C: HashedCursor> HashedCursor for OverlayCursor<C> {
    type Value = C::Value;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, C::Value)>, TrieError> {
        self.base_next = if self.wiped { None } else { self.base.seek(key)? };
        self.index = self.overlay.partition_point(|(k, _)| *k < key);
        self.merged()
    }

    fn next(&mut self) -> Result<Option<(B256, C::Value)>, TrieError> {
        self.merged()
    }
}

/// A hashed cursor with no entries.
#[derive(Debug, Default)]
pub struct EmptyCursor<V>(pub std::marker::PhantomData<V>);

impl<V: Clone> HashedCursor for EmptyCursor<V> {
    type Value = V;

    fn seek(&mut self, _key: B256) -> Result<Option<(B256, V)>, TrieError> {
        Ok(None)
    }

    fn next(&mut self) -> Result<Option<(B256, V)>, TrieError> {
        Ok(None)
    }
}
