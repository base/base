use alloy_primitives::{B256, U256};
use reth_db::{
    DatabaseError,
    table::{Decode, Decompress, Encode},
};
use reth_primitives_traits::Account;
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};
use reth_trie_common::{BranchNodeCompact, Nibbles, StoredNibbles};
use rocksdb::DB;
use std::sync::Arc;

use crate::db::{HashedStorageKey, MaybeDeleted, StorageTrieKey, StorageValue};

use super::cf::{
    CF_ACCOUNT_TRIE_HISTORY, CF_HASHED_ACCOUNT_HISTORY, CF_HASHED_STORAGE_HISTORY,
    CF_STORAGE_TRIE_HISTORY, encode_composite_key, encode_key_ceiling, encode_key_floor,
    key_prefix_matches,
};

const BLOCK_NUMBER_LEN: usize = 8;

fn cf_not_found_error(cf_name: &str) -> DatabaseError {
    DatabaseError::Other(format!("missing RocksDB column family: {cf_name}"))
}

fn composite_prefix(raw_key: &[u8]) -> Result<&[u8], DatabaseError> {
    if raw_key.len() < BLOCK_NUMBER_LEN {
        return Err(DatabaseError::Decode);
    }
    Ok(&raw_key[..raw_key.len() - BLOCK_NUMBER_LEN])
}

fn latest_raw_value_for_prefix(
    db: &DB,
    cf_name: &str,
    key_prefix: &[u8],
    max_block_number: u64,
) -> Result<Option<Vec<u8>>, DatabaseError> {
    let cf = db.cf_handle(cf_name).ok_or_else(|| cf_not_found_error(cf_name))?;
    let mut iter = db.raw_iterator_cf(&cf);
    let seek_target = encode_composite_key(key_prefix, max_block_number);
    iter.seek_for_prev(&seek_target);

    while iter.valid() {
        let Some(found_key) = iter.key() else {
            return Ok(None);
        };

        if key_prefix_matches(found_key, key_prefix) {
            let Some(found_value) = iter.value() else {
                return Err(DatabaseError::Decode);
            };
            return Ok(Some(found_value.to_vec()));
        }

        if found_key.len() < key_prefix.len()
            || &found_key[..key_prefix.len()] != key_prefix
        {
            return Ok(None);
        }

        iter.prev();
    }

    Ok(None)
}

fn first_prefix_at_or_after(
    db: &DB,
    cf_name: &str,
    start_prefix: &[u8],
) -> Result<Option<Vec<u8>>, DatabaseError> {
    let cf = db.cf_handle(cf_name).ok_or_else(|| cf_not_found_error(cf_name))?;
    let mut iter = db.raw_iterator_cf(&cf);
    iter.seek(encode_key_floor(start_prefix));

    if !iter.valid() {
        return Ok(None);
    }

    let Some(found_key) = iter.key() else {
        return Ok(None);
    };
    Ok(Some(composite_prefix(found_key)?.to_vec()))
}

fn next_prefix_after(
    db: &DB,
    cf_name: &str,
    current_prefix: &[u8],
) -> Result<Option<Vec<u8>>, DatabaseError> {
    let cf = db.cf_handle(cf_name).ok_or_else(|| cf_not_found_error(cf_name))?;
    let mut iter = db.raw_iterator_cf(&cf);

    // Seek to the first entry at or after the floor of current_prefix,
    // then skip all entries whose prefix is exactly current_prefix.
    // Variable-length keys mean current_prefix can be a byte-prefix
    // of other valid keys (e.g. nibbles [1]=[0x01] vs [1,2]=[0x01,0x02]),
    // so we must NOT skip those — only skip exact-length matches.
    iter.seek(encode_key_floor(current_prefix));

    while iter.valid() {
        let Some(found_key) = iter.key() else { break };
        let found_prefix = composite_prefix(found_key)?;
        if found_prefix != current_prefix {
            return Ok(Some(found_prefix.to_vec()));
        }
        iter.next();
    }

    Ok(None)
}

fn prefix_in_scope(prefix: &[u8], scope_prefix: Option<&[u8]>) -> bool {
    scope_prefix.is_none_or(|scope| prefix.starts_with(scope))
}

fn seek_live_from_prefix<V, R, F>(
    db: &DB,
    cf_name: &str,
    max_block_number: u64,
    start_prefix: &[u8],
    scope_prefix: Option<&[u8]>,
    mut map_value: F,
) -> Result<Option<(Vec<u8>, R)>, DatabaseError>
where
    V: Decompress,
    F: FnMut(V) -> Option<R>,
{
    let mut candidate = first_prefix_at_or_after(db, cf_name, start_prefix)?;

    while let Some(prefix) = candidate {
        if !prefix_in_scope(&prefix, scope_prefix) {
            return Ok(None);
        }

        if let Some(raw_value) = latest_raw_value_for_prefix(db, cf_name, &prefix, max_block_number)? {
            let MaybeDeleted(value) = MaybeDeleted::<V>::decompress(&raw_value)?;
            if let Some(value) = value
                && let Some(mapped) = map_value(value)
            {
                return Ok(Some((prefix, mapped)));
            }
        }

        candidate = next_prefix_after(db, cf_name, &prefix)?;
    }

    Ok(None)
}

fn next_live_after_prefix<V, R, F>(
    db: &DB,
    cf_name: &str,
    max_block_number: u64,
    current_prefix: &[u8],
    scope_prefix: Option<&[u8]>,
    mut map_value: F,
) -> Result<Option<(Vec<u8>, R)>, DatabaseError>
where
    V: Decompress,
    F: FnMut(V) -> Option<R>,
{
    let mut candidate = next_prefix_after(db, cf_name, current_prefix)?;

    while let Some(prefix) = candidate {
        if !prefix_in_scope(&prefix, scope_prefix) {
            return Ok(None);
        }

        if let Some(raw_value) = latest_raw_value_for_prefix(db, cf_name, &prefix, max_block_number)? {
            let MaybeDeleted(value) = MaybeDeleted::<V>::decompress(&raw_value)?;
            if let Some(value) = value
                && let Some(mapped) = map_value(value)
            {
                return Ok(Some((prefix, mapped)));
            }
        }

        candidate = next_prefix_after(db, cf_name, &prefix)?;
    }

    Ok(None)
}

#[derive(Debug)]
pub struct RocksdbTrieCursor {
    db: Arc<DB>,
    cf_name: &'static str,
    max_block_number: u64,
    hashed_address: Option<B256>,
    current_prefix: Option<Vec<u8>>,
}

impl RocksdbTrieCursor {
    pub fn new_account(db: Arc<DB>, max_block_number: u64) -> Self {
        Self {
            db,
            cf_name: CF_ACCOUNT_TRIE_HISTORY,
            max_block_number,
            hashed_address: None,
            current_prefix: None,
        }
    }

    pub fn new_storage(db: Arc<DB>, hashed_address: B256, max_block_number: u64) -> Self {
        Self {
            db,
            cf_name: CF_STORAGE_TRIE_HISTORY,
            max_block_number,
            hashed_address: Some(hashed_address),
            current_prefix: None,
        }
    }

    fn is_storage(&self) -> bool {
        self.cf_name == CF_STORAGE_TRIE_HISTORY
    }

    fn scope_prefix(&self) -> Option<Vec<u8>> {
        self.hashed_address.map(|address| address.as_slice().to_vec())
    }

    fn encode_seek_prefix(&self, path: Nibbles) -> Vec<u8> {
        if self.is_storage()
            && let Some(address) = self.hashed_address
        {
            return StorageTrieKey::new(address, StoredNibbles(path)).encode();
        }
        StoredNibbles(path).encode().to_vec()
    }

    fn decode_path(&self, key_prefix: &[u8]) -> Result<Nibbles, DatabaseError> {
        if self.is_storage() {
            let key = StorageTrieKey::decode(key_prefix)?;
            if self.hashed_address.is_some_and(|address| key.hashed_address != address) {
                return Err(DatabaseError::Decode);
            }
            return Ok(key.path.0);
        }

        Ok(StoredNibbles::decode(key_prefix)?.0)
    }

    fn initial_start_prefix(&self) -> Option<Vec<u8>> {
        if self.is_storage() {
            let address = self.hashed_address?;
            Some(StorageTrieKey::new(address, StoredNibbles(Nibbles::default())).encode())
        } else {
            Some(StoredNibbles(Nibbles::default()).encode().to_vec())
        }
    }
}

impl TrieCursor for RocksdbTrieCursor {
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let key_prefix = self.encode_seek_prefix(path);
        let Some(raw_value) = latest_raw_value_for_prefix(
            &self.db,
            self.cf_name,
            &key_prefix,
            self.max_block_number,
        )? else {
            return Ok(None);
        };

        let MaybeDeleted(value) = MaybeDeleted::<BranchNodeCompact>::decompress(&raw_value)?;
        let Some(node) = value else {
            return Ok(None);
        };

        let path = self.decode_path(&key_prefix)?;
        self.current_prefix = Some(key_prefix);
        Ok(Some((path, node)))
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if self.is_storage() && self.hashed_address.is_none() {
            return Ok(None);
        }

        let start_prefix = self.encode_seek_prefix(path);
        let scope = self.scope_prefix();
        let found = seek_live_from_prefix::<BranchNodeCompact, _, _>(
            &self.db,
            self.cf_name,
            self.max_block_number,
            &start_prefix,
            scope.as_deref(),
            Some,
        )?;

        if let Some((prefix, node)) = found {
            let path = self.decode_path(&prefix)?;
            self.current_prefix = Some(prefix);
            return Ok(Some((path, node)));
        }

        Ok(None)
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if self.is_storage() && self.hashed_address.is_none() {
            return Ok(None);
        }

        let scope = self.scope_prefix();
        let found = if let Some(current_prefix) = self.current_prefix.as_deref() {
            next_live_after_prefix::<BranchNodeCompact, _, _>(
                &self.db,
                self.cf_name,
                self.max_block_number,
                current_prefix,
                scope.as_deref(),
                Some,
            )?
        } else if let Some(start_prefix) = self.initial_start_prefix() {
            seek_live_from_prefix::<BranchNodeCompact, _, _>(
                &self.db,
                self.cf_name,
                self.max_block_number,
                &start_prefix,
                scope.as_deref(),
                Some,
            )?
        } else {
            None
        };

        if let Some((prefix, node)) = found {
            let path = self.decode_path(&prefix)?;
            self.current_prefix = Some(prefix);
            return Ok(Some((path, node)));
        }

        Ok(None)
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        self.current_prefix
            .as_deref()
            .map(|prefix| self.decode_path(prefix))
            .transpose()
    }

    fn reset(&mut self) {
        self.current_prefix = None;
    }
}

impl TrieStorageCursor for RocksdbTrieCursor {
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.cf_name = CF_STORAGE_TRIE_HISTORY;
        self.hashed_address = Some(hashed_address);
        self.reset();
    }
}

#[derive(Debug)]
pub struct RocksdbStorageCursor {
    db: Arc<DB>,
    max_block_number: u64,
    hashed_address: B256,
    current_prefix: Option<Vec<u8>>,
}

impl RocksdbStorageCursor {
    pub fn new(db: Arc<DB>, hashed_address: B256, max_block_number: u64) -> Self {
        Self { db, max_block_number, hashed_address, current_prefix: None }
    }

    fn scope_prefix(&self) -> Vec<u8> {
        self.hashed_address.as_slice().to_vec()
    }

    fn encode_seek_prefix(&self, key: B256) -> Vec<u8> {
        HashedStorageKey::new(self.hashed_address, key).encode().to_vec()
    }

    fn decode_slot(&self, key_prefix: &[u8]) -> Result<B256, DatabaseError> {
        let key = HashedStorageKey::decode(key_prefix)?;
        if key.hashed_address != self.hashed_address {
            return Err(DatabaseError::Decode);
        }
        Ok(key.hashed_storage_key)
    }

    fn initial_start_prefix(&self) -> Vec<u8> {
        HashedStorageKey::new(self.hashed_address, B256::ZERO).encode().to_vec()
    }
}

impl HashedCursor for RocksdbStorageCursor {
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let start_prefix = self.encode_seek_prefix(key);
        let scope = self.scope_prefix();
        let found = seek_live_from_prefix::<StorageValue, _, _>(
            &self.db,
            CF_HASHED_STORAGE_HISTORY,
            self.max_block_number,
            &start_prefix,
            Some(scope.as_slice()),
            |value| (!value.0.is_zero()).then_some(value.0),
        )?;

        if let Some((prefix, value)) = found {
            let key = self.decode_slot(&prefix)?;
            self.current_prefix = Some(prefix);
            return Ok(Some((key, value)));
        }

        Ok(None)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let scope = self.scope_prefix();
        let found = if let Some(current_prefix) = self.current_prefix.as_deref() {
            next_live_after_prefix::<StorageValue, _, _>(
                &self.db,
                CF_HASHED_STORAGE_HISTORY,
                self.max_block_number,
                current_prefix,
                Some(scope.as_slice()),
                |value| (!value.0.is_zero()).then_some(value.0),
            )?
        } else {
            let start_prefix = self.initial_start_prefix();
            seek_live_from_prefix::<StorageValue, _, _>(
                &self.db,
                CF_HASHED_STORAGE_HISTORY,
                self.max_block_number,
                &start_prefix,
                Some(scope.as_slice()),
                |value| (!value.0.is_zero()).then_some(value.0),
            )?
        };

        if let Some((prefix, value)) = found {
            let key = self.decode_slot(&prefix)?;
            self.current_prefix = Some(prefix);
            return Ok(Some((key, value)));
        }

        Ok(None)
    }

    fn reset(&mut self) {
        self.current_prefix = None;
    }
}

impl HashedStorageCursor for RocksdbStorageCursor {
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        Ok(self.seek(B256::ZERO)?.is_none())
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.reset();
    }
}

#[derive(Debug)]
pub struct RocksdbAccountCursor {
    db: Arc<DB>,
    max_block_number: u64,
    current_prefix: Option<Vec<u8>>,
}

impl RocksdbAccountCursor {
    pub fn new(db: Arc<DB>, max_block_number: u64) -> Self {
        Self { db, max_block_number, current_prefix: None }
    }

    fn decode_address(key_prefix: &[u8]) -> Result<B256, DatabaseError> {
        if key_prefix.len() != 32 {
            return Err(DatabaseError::Decode);
        }
        Ok(B256::from_slice(key_prefix))
    }
}

impl HashedCursor for RocksdbAccountCursor {
    type Value = Account;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let found = seek_live_from_prefix::<Account, _, _>(
            &self.db,
            CF_HASHED_ACCOUNT_HISTORY,
            self.max_block_number,
            key.as_slice(),
            None,
            Some,
        )?;

        if let Some((prefix, value)) = found {
            let key = Self::decode_address(&prefix)?;
            self.current_prefix = Some(prefix);
            return Ok(Some((key, value)));
        }

        Ok(None)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let found = if let Some(current_prefix) = self.current_prefix.as_deref() {
            next_live_after_prefix::<Account, _, _>(
                &self.db,
                CF_HASHED_ACCOUNT_HISTORY,
                self.max_block_number,
                current_prefix,
                None,
                Some,
            )?
        } else {
            seek_live_from_prefix::<Account, _, _>(
                &self.db,
                CF_HASHED_ACCOUNT_HISTORY,
                self.max_block_number,
                B256::ZERO.as_slice(),
                None,
                Some,
            )?
        };

        if let Some((prefix, value)) = found {
            let key = Self::decode_address(&prefix)?;
            self.current_prefix = Some(prefix);
            return Ok(Some((key, value)));
        }

        Ok(None)
    }

    fn reset(&mut self) {
        self.current_prefix = None;
    }
}
