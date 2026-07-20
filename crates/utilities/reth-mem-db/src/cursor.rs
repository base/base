use std::{marker::PhantomData, ops::Bound};

use reth_db_api::{
    DatabaseError,
    common::{PairResult, ValueOnlyResult},
    cursor::{
        DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, DupWalker, RangeWalker,
        ReverseWalker, Walker,
    },
    table::{Compress, Decode, Decompress, DupSort, Encode, IntoVec, Table},
};

use crate::db::{SharedStore, TableData};

fn build_entries(data: &TableData) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for (pk, inner) in data {
        for cv in inner.values() {
            out.push((pk.clone(), cv.clone()));
        }
    }
    out
}

fn decode_pair<T: Table>(pk: &[u8], cv: &[u8]) -> Result<(T::Key, T::Value), DatabaseError> {
    let key = T::Key::decode(pk)?;
    let value = T::Value::decompress(cv).map_err(DatabaseError::from)?;
    Ok((key, value))
}

fn cursor_at<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
    idx: usize,
) -> PairResult<T> {
    if idx >= entries.len() {
        *pos = None;
        return Ok(None);
    }
    *pos = Some(idx);
    let (pk, cv) = &entries[idx];
    decode_pair::<T>(pk, cv).map(Some)
}

fn cursor_first<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> PairResult<T> {
    cursor_at::<T>(entries, pos, 0)
}

fn cursor_last<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> PairResult<T> {
    if entries.is_empty() {
        *pos = None;
        return Ok(None);
    }
    cursor_at::<T>(entries, pos, entries.len() - 1)
}

fn cursor_next<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> PairResult<T> {
    let idx = pos.map(|p| p + 1).unwrap_or(0);
    cursor_at::<T>(entries, pos, idx)
}

fn cursor_prev<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> PairResult<T> {
    match *pos {
        None | Some(0) => {
            *pos = None;
            Ok(None)
        }
        Some(p) => cursor_at::<T>(entries, pos, p - 1),
    }
}

fn cursor_current<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &Option<usize>,
) -> PairResult<T> {
    match *pos {
        None => Ok(None),
        Some(p) if p >= entries.len() => Ok(None),
        Some(p) => {
            let (pk, cv) = &entries[p];
            decode_pair::<T>(pk, cv).map(Some)
        }
    }
}

fn cursor_seek_exact<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
    key: T::Key,
) -> PairResult<T> {
    let encoded = key.encode().into_vec();
    let idx = entries.partition_point(|(pk, _)| pk.as_slice() < encoded.as_slice());
    if idx < entries.len() && entries[idx].0 == encoded {
        cursor_at::<T>(entries, pos, idx)
    } else {
        *pos = None;
        Ok(None)
    }
}

fn cursor_seek<T: Table>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
    key: T::Key,
) -> PairResult<T> {
    let encoded = key.encode().into_vec();
    let idx = entries.partition_point(|(pk, _)| pk.as_slice() < encoded.as_slice());
    cursor_at::<T>(entries, pos, idx)
}

fn dup_prev<T: DupSort>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> PairResult<T> {
    let p = match *pos {
        None | Some(0) => return Ok(None),
        Some(p) => p,
    };
    if p >= entries.len() {
        return Ok(None);
    }
    let current_pk = &entries[p].0;
    if entries[p - 1].0 != *current_pk {
        return Ok(None);
    }
    cursor_at::<T>(entries, pos, p - 1)
}

fn dup_next<T: DupSort>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> PairResult<T> {
    let p = match *pos {
        None => return Ok(None),
        Some(p) => p,
    };
    if p >= entries.len() {
        return Ok(None);
    }
    let current_pk = &entries[p].0;
    let next = p + 1;
    if next >= entries.len() || entries[next].0 != *current_pk {
        return Ok(None);
    }
    cursor_at::<T>(entries, pos, next)
}

fn dup_last<T: DupSort>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> ValueOnlyResult<T> {
    let p = match *pos {
        None => return Ok(None),
        Some(p) if p >= entries.len() => return Ok(None),
        Some(p) => p,
    };
    let current_pk = &entries[p].0.clone();
    let mut last = p;
    while last + 1 < entries.len() && entries[last + 1].0 == *current_pk {
        last += 1;
    }
    *pos = Some(last);
    let (_, cv) = &entries[last];
    T::Value::decompress(cv).map(Some).map_err(DatabaseError::from)
}

fn dup_next_no_dup<T: DupSort>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
) -> PairResult<T> {
    let p = match *pos {
        None => return cursor_first::<T>(entries, pos),
        Some(p) => p,
    };
    if p >= entries.len() {
        *pos = None;
        return Ok(None);
    }
    let current_pk = entries[p].0.clone();
    let next = entries[p..].iter().position(|(pk, _)| *pk != current_pk).map(|off| p + off);
    match next {
        None => {
            *pos = None;
            Ok(None)
        }
        Some(idx) => cursor_at::<T>(entries, pos, idx),
    }
}

fn dup_seek_by_key_subkey<T: DupSort>(
    entries: &[(Vec<u8>, Vec<u8>)],
    pos: &mut Option<usize>,
    key: T::Key,
    subkey: T::SubKey,
) -> ValueOnlyResult<T> {
    let pk = key.encode().into_vec();
    let sk = subkey.encode().into_vec();
    let idx = entries.partition_point(|(k, cv)| {
        k.as_slice() < pk.as_slice()
            || (k.as_slice() == pk.as_slice() && cv.as_slice() < sk.as_slice())
    });
    if idx >= entries.len() || entries[idx].0 != pk {
        *pos = None;
        return Ok(None);
    }
    *pos = Some(idx);
    let (_, cv) = &entries[idx];
    T::Value::decompress(cv).map(Some).map_err(DatabaseError::from)
}

fn walk_dup_start<T, C>(
    cursor: &mut C,
    key: Option<T::Key>,
    subkey: Option<T::SubKey>,
) -> Result<reth_db_api::common::IterPairResult<T>, DatabaseError>
where
    T: DupSort,
    C: DbCursorRO<T> + DbDupCursorRO<T>,
{
    let start = match (key, subkey) {
        (None, None) => cursor.first().transpose(),
        (Some(k), None) => cursor.seek_exact(k).transpose(),
        (None, Some(sk)) => match cursor.first()? {
            None => None,
            Some((fk, _)) => match cursor.seek_by_key_subkey(fk, sk)? {
                None => None,
                Some(v) => cursor.current()?.map(|(k, _)| Ok((k, v))),
            },
        },
        (Some(k), Some(sk)) => match cursor.seek_by_key_subkey(k, sk)? {
            None => None,
            Some(v) => cursor.current()?.map(|(k, _)| Ok((k, v))),
        },
    };
    Ok(start)
}

/// Read-only cursor backed by a frozen snapshot of one table.
pub struct MemCursor<T: Table> {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos: Option<usize>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: Table> std::fmt::Debug for MemCursor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemCursor")
            .field("len", &self.entries.len())
            .field("pos", &self.pos)
            .finish()
    }
}

impl<T: Table> MemCursor<T> {
    /// Create a read-only cursor from a table snapshot.
    pub fn new(data: TableData) -> Self {
        Self { entries: build_entries(&data), pos: None, _marker: PhantomData }
    }
}

impl<T: Table> DbCursorRO<T> for MemCursor<T> {
    fn first(&mut self) -> PairResult<T> {
        cursor_first::<T>(&self.entries, &mut self.pos)
    }

    fn seek_exact(&mut self, key: T::Key) -> PairResult<T> {
        cursor_seek_exact::<T>(&self.entries, &mut self.pos, key)
    }

    fn seek(&mut self, key: T::Key) -> PairResult<T> {
        cursor_seek::<T>(&self.entries, &mut self.pos, key)
    }

    fn next(&mut self) -> PairResult<T> {
        cursor_next::<T>(&self.entries, &mut self.pos)
    }

    fn prev(&mut self) -> PairResult<T> {
        cursor_prev::<T>(&self.entries, &mut self.pos)
    }

    fn last(&mut self) -> PairResult<T> {
        cursor_last::<T>(&self.entries, &mut self.pos)
    }

    fn current(&mut self) -> PairResult<T> {
        cursor_current::<T>(&self.entries, &self.pos)
    }

    fn walk(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<Walker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match start_key {
            None => self.first().transpose(),
            Some(k) => self.seek(k).transpose(),
        };
        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl std::ops::RangeBounds<T::Key>,
    ) -> Result<RangeWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match range.start_bound() {
            Bound::Included(k) => {
                cursor_seek::<T>(&self.entries, &mut self.pos, k.clone()).transpose()
            }
            Bound::Excluded(k) => {
                let encoded = k.clone().encode().into_vec();
                let idx =
                    self.entries.partition_point(|(pk, _)| pk.as_slice() <= encoded.as_slice());
                cursor_at::<T>(&self.entries, &mut self.pos, idx).transpose()
            }
            Bound::Unbounded => cursor_first::<T>(&self.entries, &mut self.pos).transpose(),
        };
        let end_key = match range.end_bound() {
            Bound::Included(k) => Bound::Included(k.clone()),
            Bound::Excluded(k) => Bound::Excluded(k.clone()),
            Bound::Unbounded => Bound::Unbounded,
        };
        Ok(RangeWalker::new(self, start, end_key))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<ReverseWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match start_key {
            None => self.last().transpose(),
            Some(k) => self.seek(k).transpose(),
        };
        Ok(ReverseWalker::new(self, start))
    }
}

impl<T: DupSort> DbDupCursorRO<T> for MemCursor<T> {
    fn prev_dup(&mut self) -> PairResult<T> {
        dup_prev::<T>(&self.entries, &mut self.pos)
    }

    fn next_dup(&mut self) -> PairResult<T> {
        dup_next::<T>(&self.entries, &mut self.pos)
    }

    fn last_dup(&mut self) -> ValueOnlyResult<T> {
        dup_last::<T>(&self.entries, &mut self.pos)
    }

    fn next_no_dup(&mut self) -> PairResult<T> {
        dup_next_no_dup::<T>(&self.entries, &mut self.pos)
    }

    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        Ok(self.next_dup()?.map(|(_, v)| v))
    }

    fn seek_by_key_subkey(&mut self, key: T::Key, subkey: T::SubKey) -> ValueOnlyResult<T> {
        dup_seek_by_key_subkey::<T>(&self.entries, &mut self.pos, key, subkey)
    }

    fn walk_dup(
        &mut self,
        key: Option<T::Key>,
        subkey: Option<T::SubKey>,
    ) -> Result<DupWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = walk_dup_start(self, key, subkey)?;
        Ok(DupWalker { cursor: self, start })
    }
}

/// Writable cursor that flushes every mutation immediately to the shared store.
pub struct MemCursorMut<T: Table> {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos: Option<usize>,
    data: TableData,
    table_name: &'static str,
    store: SharedStore,
    _marker: PhantomData<fn() -> T>,
}

impl<T: Table> std::fmt::Debug for MemCursorMut<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemCursorMut")
            .field("len", &self.entries.len())
            .field("pos", &self.pos)
            .finish()
    }
}

impl<T: Table> MemCursorMut<T> {
    /// Create a writable cursor for the given `table_name` backed by `data`.
    pub fn new(data: TableData, table_name: &'static str, store: SharedStore) -> Self {
        let entries = build_entries(&data);
        Self { entries, pos: None, data, table_name, store, _marker: PhantomData }
    }

    fn flush(&mut self) {
        self.entries = build_entries(&self.data);
        let mut guard = self.store.write();
        guard.insert(self.table_name, self.data.clone());
    }

    fn put_entry(&mut self, pk: Vec<u8>, cv: Vec<u8>) {
        let inner_key = if T::DUPSORT { cv.clone() } else { vec![] };
        self.data.entry(pk.clone()).or_default().insert(inner_key, cv.clone());
        self.flush();
        self.pos = self.entries.iter().rposition(|(k, v)| k == &pk && v == &cv);
    }
}

impl<T: Table> DbCursorRO<T> for MemCursorMut<T> {
    fn first(&mut self) -> PairResult<T> {
        cursor_first::<T>(&self.entries, &mut self.pos)
    }

    fn seek_exact(&mut self, key: T::Key) -> PairResult<T> {
        cursor_seek_exact::<T>(&self.entries, &mut self.pos, key)
    }

    fn seek(&mut self, key: T::Key) -> PairResult<T> {
        cursor_seek::<T>(&self.entries, &mut self.pos, key)
    }

    fn next(&mut self) -> PairResult<T> {
        cursor_next::<T>(&self.entries, &mut self.pos)
    }

    fn prev(&mut self) -> PairResult<T> {
        cursor_prev::<T>(&self.entries, &mut self.pos)
    }

    fn last(&mut self) -> PairResult<T> {
        cursor_last::<T>(&self.entries, &mut self.pos)
    }

    fn current(&mut self) -> PairResult<T> {
        cursor_current::<T>(&self.entries, &self.pos)
    }

    fn walk(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<Walker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match start_key {
            None => self.first().transpose(),
            Some(k) => self.seek(k).transpose(),
        };
        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl std::ops::RangeBounds<T::Key>,
    ) -> Result<RangeWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match range.start_bound() {
            Bound::Included(k) => {
                cursor_seek::<T>(&self.entries, &mut self.pos, k.clone()).transpose()
            }
            Bound::Excluded(k) => {
                let encoded = k.clone().encode().into_vec();
                let idx =
                    self.entries.partition_point(|(pk, _)| pk.as_slice() <= encoded.as_slice());
                cursor_at::<T>(&self.entries, &mut self.pos, idx).transpose()
            }
            Bound::Unbounded => cursor_first::<T>(&self.entries, &mut self.pos).transpose(),
        };
        let end_key = match range.end_bound() {
            Bound::Included(k) => Bound::Included(k.clone()),
            Bound::Excluded(k) => Bound::Excluded(k.clone()),
            Bound::Unbounded => Bound::Unbounded,
        };
        Ok(RangeWalker::new(self, start, end_key))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<ReverseWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match start_key {
            None => self.last().transpose(),
            Some(k) => self.seek(k).transpose(),
        };
        Ok(ReverseWalker::new(self, start))
    }
}

impl<T: DupSort> DbDupCursorRO<T> for MemCursorMut<T> {
    fn prev_dup(&mut self) -> PairResult<T> {
        dup_prev::<T>(&self.entries, &mut self.pos)
    }

    fn next_dup(&mut self) -> PairResult<T> {
        dup_next::<T>(&self.entries, &mut self.pos)
    }

    fn last_dup(&mut self) -> ValueOnlyResult<T> {
        dup_last::<T>(&self.entries, &mut self.pos)
    }

    fn next_no_dup(&mut self) -> PairResult<T> {
        dup_next_no_dup::<T>(&self.entries, &mut self.pos)
    }

    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        Ok(self.next_dup()?.map(|(_, v)| v))
    }

    fn seek_by_key_subkey(&mut self, key: T::Key, subkey: T::SubKey) -> ValueOnlyResult<T> {
        dup_seek_by_key_subkey::<T>(&self.entries, &mut self.pos, key, subkey)
    }

    fn walk_dup(
        &mut self,
        key: Option<T::Key>,
        subkey: Option<T::SubKey>,
    ) -> Result<DupWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = walk_dup_start(self, key, subkey)?;
        Ok(DupWalker { cursor: self, start })
    }
}

impl<T: Table> DbCursorRW<T> for MemCursorMut<T> {
    fn upsert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        let pk = key.encode().into_vec();
        let mut cv = Vec::new();
        value.compress_to_buf(&mut cv);
        self.put_entry(pk, cv);
        Ok(())
    }

    fn insert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        let pk = key.encode().into_vec();
        let mut cv = Vec::new();
        value.compress_to_buf(&mut cv);
        let inner_key = if T::DUPSORT { cv.clone() } else { vec![] };
        if self.data.get(&pk).is_some_and(|inner| inner.contains_key(&inner_key)) {
            return Err(DatabaseError::Other("key already exists".into()));
        }
        self.put_entry(pk, cv);
        Ok(())
    }

    fn append(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.upsert(key, value)
    }

    fn delete_current(&mut self) -> Result<(), DatabaseError> {
        let p = match self.pos {
            None => return Ok(()),
            Some(p) if p >= self.entries.len() => return Ok(()),
            Some(p) => p,
        };
        let (pk, cv) = self.entries[p].clone();
        let inner_key = if T::DUPSORT { cv } else { vec![] };
        if let Some(inner) = self.data.get_mut(&pk) {
            inner.remove(&inner_key);
            if inner.is_empty() {
                self.data.remove(&pk);
            }
        }
        self.flush();
        if p >= self.entries.len() {
            self.pos = None;
        }
        Ok(())
    }
}

impl<T: DupSort> DbDupCursorRW<T> for MemCursorMut<T> {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        let p = match self.pos {
            None => return Ok(()),
            Some(p) if p >= self.entries.len() => return Ok(()),
            Some(p) => p,
        };
        let current_pk = self.entries[p].0.clone();
        self.data.remove(&current_pk);
        self.flush();
        let next =
            self.entries.partition_point(|(pk, _)| pk.as_slice() <= current_pk.as_slice());
        self.pos = if next < self.entries.len() { Some(next) } else { None };
        Ok(())
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let pk = key.encode().into_vec();
        let mut cv = Vec::new();
        value.compress_to_buf(&mut cv);
        self.data.entry(pk.clone()).or_default().insert(cv.clone(), cv.clone());
        self.flush();
        self.pos = self.entries.iter().rposition(|(k, v)| k == &pk && v == &cv);
        Ok(())
    }
}
