use std::{collections::BTreeMap, sync::Arc};

use parking_lot::Mutex;
use reth_db_api::{
    DatabaseError,
    table::{Compress, Decompress, DupSort, Encode, IntoVec, Table},
    transaction::{DbTx, DbTxMut},
};

use crate::{
    cursor::{MemCursor, MemCursorMut},
    db::SharedStore,
};

type PendingMap = BTreeMap<&'static str, BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>>;
type DeletesMap = BTreeMap<&'static str, Vec<(Vec<u8>, Option<Vec<u8>>)>>;

/// Read-only transaction: holds a snapshot of the store.
#[derive(Debug)]
pub struct MemTx {
    snapshot: BTreeMap<&'static str, BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl MemTx {
    /// Create a read-only transaction by snapshotting the store.
    pub fn new(store: SharedStore) -> Self {
        let guard = store.read();
        Self { snapshot: guard.clone() }
    }

    fn get_row<T: Table>(&self, encoded_key: &[u8]) -> Result<Option<T::Value>, DatabaseError> {
        let table = match self.snapshot.get(T::NAME) {
            Some(t) => t,
            None => return Ok(None),
        };
        let submap = match table.get(encoded_key) {
            Some(m) => m,
            None => return Ok(None),
        };
        match submap.values().next() {
            None => Ok(None),
            Some(cv) => Ok(Some(T::Value::decompress(cv).map_err(DatabaseError::from)?)),
        }
    }
}

impl DbTx for MemTx {
    type Cursor<T: Table> = MemCursor<T>;
    type DupCursor<T: DupSort> = MemCursor<T>;

    fn get<T: Table>(&self, key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        let encoded = key.encode().into_vec();
        self.get_row::<T>(&encoded)
    }

    fn get_by_encoded_key<T: Table>(
        &self,
        key: &<T::Key as Encode>::Encoded,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.get_row::<T>(key.as_ref())
    }

    fn commit(self) -> Result<(), DatabaseError> {
        Ok(())
    }

    fn abort(self) {}

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        let table = self.snapshot.get(T::NAME).cloned().unwrap_or_default();
        Ok(MemCursor::new(table))
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        let table = self.snapshot.get(T::NAME).cloned().unwrap_or_default();
        Ok(MemCursor::new(table))
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        Ok(self.snapshot.get(T::NAME).map(|t| t.len()).unwrap_or(0))
    }

    fn disable_long_read_transaction_safety(&mut self) {}
}

/// Read-write transaction: buffers writes and commits them atomically.
pub struct MemTxMut {
    store: SharedStore,
    pending: Mutex<PendingMap>,
    deletes: Mutex<DeletesMap>,
}

impl std::fmt::Debug for MemTxMut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemTxMut")
            .field("pending_tables", &self.pending.lock().len())
            .finish()
    }
}

impl MemTxMut {
    /// Create a new read-write transaction backed by the given shared store.
    pub fn new(store: SharedStore) -> Self {
        Self {
            store,
            pending: Mutex::new(BTreeMap::new()),
            deletes: Mutex::new(BTreeMap::new()),
        }
    }

    fn commit_inner(self) -> Result<(), DatabaseError> {
        let deletes = self.deletes.into_inner();
        let pending = self.pending.into_inner();
        let mut store = self.store.write();
        for (name, entries) in deletes {
            let table = store.entry(name).or_default();
            for (key, subkey) in entries {
                match subkey {
                    Some(sk) => {
                        if let Some(submap) = table.get_mut(&key) {
                            submap.remove(&sk);
                            if submap.is_empty() {
                                table.remove(&key);
                            }
                        }
                    }
                    None => {
                        table.remove(&key);
                    }
                }
            }
        }
        for (name, entries) in pending {
            let table = store.entry(name).or_default();
            for (key, submap) in entries {
                table.entry(key).or_default().extend(submap);
            }
        }
        Ok(())
    }
}

impl DbTx for MemTxMut {
    type Cursor<T: Table> = MemCursor<T>;
    type DupCursor<T: DupSort> = MemCursor<T>;

    fn get<T: Table>(&self, key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        let encoded = key.encode().into_vec();
        {
            let pending = self.pending.lock();
            if let Some(table) = pending.get(T::NAME) {
                if let Some(submap) = table.get(&encoded) {
                    if let Some(cv) = submap.values().next() {
                        return Ok(Some(
                            T::Value::decompress(cv).map_err(DatabaseError::from)?,
                        ));
                    }
                }
            }
        }
        let store = self.store.read();
        if let Some(table) = store.get(T::NAME) {
            if let Some(submap) = table.get(&encoded) {
                if let Some(cv) = submap.values().next() {
                    return Ok(Some(
                        T::Value::decompress(cv).map_err(DatabaseError::from)?,
                    ));
                }
            }
        }
        Ok(None)
    }

    fn get_by_encoded_key<T: Table>(
        &self,
        key: &<T::Key as Encode>::Encoded,
    ) -> Result<Option<T::Value>, DatabaseError> {
        let encoded = key.as_ref();
        {
            let pending = self.pending.lock();
            if let Some(table) = pending.get(T::NAME) {
                if let Some(submap) = table.get(encoded) {
                    if let Some(cv) = submap.values().next() {
                        return Ok(Some(
                            T::Value::decompress(cv).map_err(DatabaseError::from)?,
                        ));
                    }
                }
            }
        }
        let store = self.store.read();
        if let Some(table) = store.get(T::NAME) {
            if let Some(submap) = table.get(encoded) {
                if let Some(cv) = submap.values().next() {
                    return Ok(Some(
                        T::Value::decompress(cv).map_err(DatabaseError::from)?,
                    ));
                }
            }
        }
        Ok(None)
    }

    fn commit(self) -> Result<(), DatabaseError> {
        self.commit_inner()
    }

    fn abort(self) {}

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        let store = self.store.read();
        let committed = store.get(T::NAME).cloned().unwrap_or_default();
        drop(store);
        let mut merged = committed;
        let pending = self.pending.lock();
        if let Some(pending_table) = pending.get(T::NAME) {
            for (k, submap) in pending_table {
                merged.entry(k.clone()).or_default().extend(submap.clone());
            }
        }
        Ok(MemCursor::new(merged))
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        self.cursor_read::<T>()
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        let store = self.store.read();
        let committed = store.get(T::NAME).map(|t| t.len()).unwrap_or(0);
        let pending = self.pending.lock();
        let pending_count = pending.get(T::NAME).map(|t| t.len()).unwrap_or(0);
        Ok(committed.max(pending_count))
    }

    fn disable_long_read_transaction_safety(&mut self) {}
}

impl DbTxMut for MemTxMut {
    type CursorMut<T: Table> = MemCursorMut<T>;
    type DupCursorMut<T: DupSort> = MemCursorMut<T>;

    fn put<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let encoded_key = key.encode().into_vec();
        let encoded_val: Vec<u8> = value.compress().into();
        let inner_key = if T::DUPSORT { encoded_val.clone() } else { vec![] };
        self.pending
            .lock()
            .entry(T::NAME)
            .or_default()
            .entry(encoded_key)
            .or_default()
            .insert(inner_key, encoded_val);
        Ok(())
    }

    fn delete<T: Table>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        let encoded_key = key.encode().into_vec();
        let inner_key = match value {
            None => None,
            Some(v) => {
                let cv: Vec<u8> = v.compress().into();
                Some(if T::DUPSORT { cv } else { vec![] })
            }
        };
        self.deletes.lock().entry(T::NAME).or_default().push((encoded_key, inner_key));
        Ok(true)
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        self.pending.lock().remove(T::NAME);
        self.store.write().remove(T::NAME);
        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        let store = self.store.read();
        let committed = store.get(T::NAME).cloned().unwrap_or_default();
        drop(store);
        Ok(MemCursorMut::new(committed, T::NAME, Arc::clone(&self.store)))
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        self.cursor_write::<T>()
    }
}

impl reth_db_api::table::TableImporter for MemTxMut {}
