//! Cursors over the datadir's trie and hashed state tables, and an in-memory overlay of hashed
//! state changes on top of them.

use std::collections::{BTreeMap, btree_map};

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

/// Changes over a base cursor, read in place from a sorted map: overlay entries replace base
/// entries with the same key, and those `value` maps to `None` delete; with `wiped`, base entries
/// are ignored.
#[derive(Debug)]
pub struct OverlayCursor<'o, C: HashedCursor, V> {
    /// The base cursor.
    pub base: C,
    /// Ignore the base.
    pub wiped: bool,
    overlay: Option<&'o BTreeMap<B256, V>>,
    value: fn(&V) -> Option<C::Value>,
    overlay_iter: Option<btree_map::Range<'o, B256, V>>,
    overlay_next: Option<(&'o B256, &'o V)>,
    base_next: Option<(B256, C::Value)>,
}

impl<'o, C: HashedCursor, V> OverlayCursor<'o, C, V> {
    /// An overlay of `overlay` changes over `base`, each read through `value`.
    pub const fn new(base: C, wiped: bool, overlay: Option<&'o BTreeMap<B256, V>>, value: fn(&V) -> Option<C::Value>) -> Self {
        Self { base, wiped, overlay, value, overlay_iter: None, overlay_next: None, base_next: None }
    }

    fn advance_overlay(&mut self) {
        self.overlay_next = self.overlay_iter.as_mut().and_then(Iterator::next);
    }

    fn merged(&mut self) -> Result<Option<(B256, C::Value)>, TrieError> {
        loop {
            let base_key = self.base_next.as_ref().map(|(k, _)| *k);
            match self.overlay_next {
                None if base_key.is_none() => return Ok(None),
                Some((o, value)) if base_key.is_none_or(|b| *o <= b) => {
                    let value = (self.value)(value);
                    self.advance_overlay();
                    if base_key == Some(*o) {
                        self.base_next = self.base.next()?;
                    }
                    if let Some(value) = value {
                        return Ok(Some((*o, value)));
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

impl<C: HashedCursor, V> HashedCursor for OverlayCursor<'_, C, V> {
    type Value = C::Value;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, C::Value)>, TrieError> {
        self.base_next = if self.wiped { None } else { self.base.seek(key)? };
        self.overlay_iter = self.overlay.map(|overlay| overlay.range(key..));
        self.advance_overlay();
        self.merged()
    }

    fn next(&mut self) -> Result<Option<(B256, C::Value)>, TrieError> {
        self.merged()
    }
}

/// Trie node changes over a base trie cursor, read in place from a sorted map: overlay nodes
/// (`None` removes) replace base nodes at the same path. The node changes of the canonical blocks
/// held in memory above the stored tip ([`crate::MemState`]).
///
/// The base cursor is read only when its next entry can decide the result: a seek the overlay
/// answers with a node at exactly the sought path reads nothing from the base, and the base's
/// entry after one it returned is read only when asked for.
#[derive(Debug)]
pub struct TrieOverlayCursor<'o, C> {
    /// The base cursor.
    pub base: C,
    overlay: Option<&'o BTreeMap<Nibbles, Option<BranchNodeCompact>>>,
    overlay_iter: Option<btree_map::Range<'o, Nibbles, Option<BranchNodeCompact>>>,
    overlay_next: Option<(&'o Nibbles, &'o Option<BranchNodeCompact>)>,
    base_next: BaseNext,
    current: Option<Nibbles>,
}

/// The base cursor's next entry, or where to read it from.
#[derive(Debug)]
enum BaseNext {
    /// Read.
    Read(Option<(Nibbles, BranchNodeCompact)>),
    /// The first entry at or after this path (not sought yet).
    From(Nibbles),
    /// The first entry after this path (not sought yet).
    After(Nibbles),
    /// The entry after the one the base cursor stands on.
    Next,
}

impl<'o, C: TrieCursor> TrieOverlayCursor<'o, C> {
    /// An overlay of `overlay` changes over `base`.
    pub const fn new(base: C, overlay: Option<&'o BTreeMap<Nibbles, Option<BranchNodeCompact>>>) -> Self {
        Self { base, overlay, overlay_iter: None, overlay_next: None, base_next: BaseNext::Read(None), current: None }
    }

    /// Reads the base cursor's next entry if not read yet.
    fn read_base(&mut self) -> Result<Option<Nibbles>, TrieError> {
        let entry = match &self.base_next {
            BaseNext::Read(entry) => return Ok(entry.as_ref().map(|(k, _)| *k)),
            BaseNext::From(key) => self.base.seek(*key)?,
            BaseNext::After(key) => {
                let key = *key;
                match self.base.seek(key)? {
                    Some((found, _)) if found == key => self.base.next()?,
                    entry => entry,
                }
            }
            BaseNext::Next => self.base.next()?,
        };
        let key = entry.as_ref().map(|(k, _)| *k);
        self.base_next = BaseNext::Read(entry);
        Ok(key)
    }

    fn merged(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        loop {
            // An overlay entry at exactly the path the base is sought from wins without reading it.
            if let (Some((o, node)), BaseNext::From(from)) = (self.overlay_next, &self.base_next)
                && o == from
            {
                self.base_next = BaseNext::After(*o);
                self.overlay_next = self.overlay_iter.as_mut().and_then(Iterator::next);
                match node {
                    Some(node) => {
                        self.current = Some(*o);
                        return Ok(Some((*o, node.clone())));
                    }
                    None => continue,
                }
            }
            let base_key = self.read_base()?;
            let entry = match self.overlay_next {
                None if base_key.is_none() => None,
                Some((o, node)) if base_key.is_none_or(|b| *o <= b) => {
                    self.overlay_next = self.overlay_iter.as_mut().and_then(Iterator::next);
                    if base_key == Some(*o) {
                        self.base_next = BaseNext::Next;
                    }
                    match node {
                        Some(node) => Some((*o, node.clone())),
                        None => continue,
                    }
                }
                _ => {
                    let BaseNext::Read(entry) = std::mem::replace(&mut self.base_next, BaseNext::Read(None)) else {
                        unreachable!("read above")
                    };
                    if entry.is_some() {
                        self.base_next = BaseNext::Next;
                    }
                    entry
                }
            };
            self.current = entry.as_ref().map(|(k, _)| *k);
            return Ok(entry);
        }
    }
}

impl<C: TrieCursor> TrieCursor for TrieOverlayCursor<'_, C> {
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
        self.base_next = BaseNext::From(key);
        self.overlay_iter = self.overlay.map(|overlay| overlay.range(key..));
        self.overlay_next = self.overlay_iter.as_mut().and_then(Iterator::next);
        self.merged()
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        self.merged()
    }

    fn current(&mut self) -> Result<Option<Nibbles>, TrieError> {
        Ok(self.current)
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

#[cfg(test)]
mod tests {
    use alloy_trie::TrieMask;

    use super::*;

    /// A base trie cursor over a sorted map, counting the reads it serves.
    struct MapCursor<'m> {
        rows: &'m BTreeMap<Nibbles, BranchNodeCompact>,
        at: Option<Nibbles>,
        reads: usize,
    }

    impl TrieCursor for MapCursor<'_> {
        fn seek_exact(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
            Ok(self.seek(key)?.filter(|(path, _)| *path == key))
        }

        fn seek(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
            self.reads += 1;
            let entry = self.rows.range(key..).next().map(|(k, v)| (*k, v.clone()));
            self.at = entry.as_ref().map(|(k, _)| *k);
            Ok(entry)
        }

        fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
            self.reads += 1;
            let at = self.at.expect("next after an entry");
            let entry = self.rows.range(at..).nth(1).map(|(k, v)| (*k, v.clone()));
            self.at = entry.as_ref().map(|(k, _)| *k);
            Ok(entry)
        }

        fn current(&mut self) -> Result<Option<Nibbles>, TrieError> {
            Ok(self.at)
        }
    }

    fn node(tag: u16) -> BranchNodeCompact {
        BranchNodeCompact::new(TrieMask::new(tag | 1), TrieMask::new(0), TrieMask::new(0), Vec::new(), None)
    }

    /// Seeks and nexts through the overlay cursor equal those through the merged map, and a seek
    /// the overlay answers at exactly the sought path reads nothing from the base.
    #[test]
    fn overlay_cursor_matches_the_merged_map_and_skips_answered_base_seeks() {
        let mut seed = 1u64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for _ in 0..300 {
            // Short paths over few nibbles, so seeks, overlay rows and base rows collide often.
            let path = |next: &mut dyn FnMut(u64) -> u64| {
                Nibbles::from_nibbles((0..1 + next(3)).map(|_| next(3) as u8).collect::<Vec<_>>())
            };
            let mut base = BTreeMap::new();
            let mut overlay = BTreeMap::new();
            for _ in 0..next(12) {
                base.insert(path(&mut next), node(next(1000) as u16));
            }
            for _ in 0..next(12) {
                let value = (next(3) > 0).then(|| node(1000 + next(1000) as u16));
                overlay.insert(path(&mut next), value);
            }
            let mut merged = base.clone();
            for (key, value) in &overlay {
                match value {
                    Some(value) => merged.insert(*key, value.clone()),
                    None => merged.remove(key),
                };
            }
            let mut cursor = TrieOverlayCursor::new(MapCursor { rows: &base, at: None, reads: 0 }, Some(&overlay));
            let mut expected: Option<Nibbles> = None;
            for _ in 0..20 {
                let (got, want) = if let Some(at) = expected.filter(|_| next(3) > 0) {
                    (cursor.next().unwrap(), merged.range(at..).nth(1).map(|(k, v)| (*k, v.clone())))
                } else {
                    let key = path(&mut next);
                    let before = cursor.base.reads;
                    let got = cursor.seek(key).unwrap();
                    if matches!(overlay.get(&key), Some(Some(_))) {
                        assert_eq!(cursor.base.reads, before, "answered seek read the base");
                    }
                    (got, merged.range(key..).next().map(|(k, v)| (*k, v.clone())))
                };
                assert_eq!(got, want);
                assert_eq!(cursor.current().unwrap(), want.as_ref().map(|(k, _)| *k));
                expected = want.map(|(k, _)| k);
            }
        }
    }
}
