//! A small safe wrapper over libmdbx: environments, transactions, tables and cursors.
//!
//! Simplified from reth's `reth-libmdbx` (crates/storage/libmdbx-rs at base/reth 2b20e42), which
//! is Apache-2.0 licensed. The environment is opened the way reth's `DatabaseEnv::open` opens it
//! (geometry, max tables, read-ahead off, coalesce, durable sync, rp augment limit), except that
//! tiger never uses `MDBX_WRITEMAP`: all page writes then go through `pwrite`, where the
//! fault-injection layer ([`crate::Fault`]) hooks them. The file format does not depend on the
//! write mode, so base-reth opens the same files.

use std::{
    ffi::{CStr, CString, c_int, c_void},
    fmt,
    marker::PhantomData,
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr, slice,
    sync::Arc,
};

use crate::mdbx_sys as ffi;

/// An MDBX error code with its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MdbxError(pub c_int);

impl MdbxError {
    /// Turns an MDBX return code into a result.
    pub fn check(code: c_int) -> Result<(), Self> {
        if code == ffi::MDBX_SUCCESS || code == ffi::MDBX_RESULT_TRUE { Ok(()) } else { Err(Self(code)) }
    }
}

impl fmt::Display for MdbxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: mdbx_strerror returns a static NUL-terminated string for any code.
        let message = unsafe { CStr::from_ptr(ffi::mdbx_strerror(self.0)) };
        write!(f, "mdbx error {}: {}", self.0, message.to_string_lossy())
    }
}

impl std::error::Error for MdbxError {}

/// Bytes in one gigabyte.
const GIGABYTE: isize = 1 << 30;
/// Bytes in one terabyte.
const TERABYTE: isize = 1 << 40;

/// How an environment is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvOptions {
    /// Open read-only.
    pub read_only: bool,
    /// Upper bound of the file size in bytes. reth uses 8 TiB.
    pub max_size: isize,
    /// File growth step in bytes. reth uses 4 GiB.
    pub growth_step: isize,
    /// Database page size for a new file. reth uses the OS page size clamped to 4096..=65536.
    pub page_size: isize,
    /// Maximum number of reader slots. reth's default is 32000.
    pub max_readers: u64,
}

impl Default for EnvOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            max_size: 8 * TERABYTE,
            growth_step: 4 * GIGABYTE,
            page_size: 4096,
            max_readers: 32_000,
        }
    }
}

/// The open environment, shared by every transaction.
#[derive(Debug)]
pub struct EnvInner {
    /// The MDBX environment handle.
    pub ptr: *mut ffi::MDBX_env,
}

// SAFETY: MDBX environments are thread safe; with MDBX_NOSTICKYTHREADS transactions may move
// between threads (each used by one thread at a time, which `&mut`/ownership enforces).
unsafe impl Send for EnvInner {}
// SAFETY: as above.
unsafe impl Sync for EnvInner {}

impl Drop for EnvInner {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and no transaction outlives the Arc holding it.
        unsafe { ffi::mdbx_env_close_ex(self.ptr, false) };
    }
}

/// An MDBX environment (one `mdbx.dat` with its lock file in a directory).
#[derive(Debug, Clone)]
pub struct Env {
    /// The shared handle.
    pub inner: Arc<EnvInner>,
}

impl Env {
    /// Opens or creates the environment in directory `path`.
    pub fn open(path: &Path, options: EnvOptions) -> Result<Self, MdbxError> {
        let mut env: *mut ffi::MDBX_env = ptr::null_mut();
        // SAFETY: plain FFI calls on a fresh handle; on error the handle is closed.
        unsafe {
            MdbxError::check(ffi::mdbx_env_create(&mut env))?;
            let setup = || -> Result<(), MdbxError> {
                MdbxError::check(ffi::mdbx_env_set_geometry(
                    env,
                    0,
                    -1,
                    options.max_size,
                    options.growth_step,
                    0,
                    options.page_size,
                ))?;
                for (option, value) in [
                    (ffi::MDBX_opt_max_db, 256),
                    (ffi::MDBX_opt_rp_augment_limit, 256 * 1024),
                    (ffi::MDBX_opt_max_readers, options.max_readers),
                ] {
                    MdbxError::check(ffi::mdbx_env_set_option(env, option, value))?;
                }
                let mut flags = ffi::MDBX_NORDAHEAD | ffi::MDBX_COALESCE | ffi::MDBX_NOSTICKYTHREADS;
                flags |= if options.read_only { ffi::MDBX_RDONLY } else { ffi::MDBX_SYNC_DURABLE };
                let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| MdbxError(ffi::MDBX_EINVAL))?;
                MdbxError::check(ffi::mdbx_env_open(env, path.as_ptr(), flags, 0o644))
            };
            if let Err(err) = setup() {
                ffi::mdbx_env_close_ex(env, false);
                return Err(err);
            }
        }
        Ok(Self { inner: Arc::new(EnvInner { ptr: env }) })
    }

    /// The page size of the open file.
    pub fn page_size(&self) -> Result<usize, MdbxError> {
        // SAFETY: MDBX_stat is plain data; the call fills it.
        let mut stat: ffi::MDBX_stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid env handle and a correctly sized out-pointer.
        MdbxError::check(unsafe {
            ffi::mdbx_env_stat_ex(self.inner.ptr, ptr::null(), &mut stat, size_of::<ffi::MDBX_stat>())
        })?;
        Ok(stat.ms_psize as usize)
    }

    /// Begins a read-only transaction.
    pub fn begin_ro(&self) -> Result<Txn<Ro>, MdbxError> {
        Txn::begin(self, ffi::MDBX_TXN_RDONLY)
    }

    /// Begins the read-write transaction, waiting for the current writer to finish.
    pub fn begin_rw(&self) -> Result<Txn<Rw>, MdbxError> {
        Txn::begin(self, ffi::MDBX_TXN_READWRITE)
    }
}

/// Marker of a read-only transaction.
#[derive(Debug)]
pub struct Ro;
/// Marker of a read-write transaction.
#[derive(Debug)]
pub struct Rw;

/// A table handle (an MDBX `dbi`), valid in every transaction of its environment once opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dbi(pub ffi::MDBX_dbi);

/// A transaction. Aborted on drop unless committed.
#[derive(Debug)]
pub struct Txn<K> {
    /// The MDBX transaction handle.
    pub ptr: *mut ffi::MDBX_txn,
    /// Keeps the environment open.
    pub env: Env,
    /// Read-only or read-write.
    pub kind: PhantomData<K>,
}

// SAFETY: with MDBX_NOSTICKYTHREADS a transaction may be used from any one thread at a time.
unsafe impl<K> Send for Txn<K> {}

/// Converts a byte slice into an MDBX value that points at it.
fn val(bytes: &[u8]) -> ffi::MDBX_val {
    ffi::MDBX_val { iov_base: bytes.as_ptr() as *mut c_void, iov_len: bytes.len() }
}

/// Borrows the bytes an MDBX value points at.
///
/// # Safety
/// The value must point at memory valid for the lifetime `'a`.
unsafe fn bytes<'a>(value: &ffi::MDBX_val) -> &'a [u8] {
    if value.iov_len == 0 {
        return &[];
    }
    // SAFETY: the caller guarantees validity for 'a.
    unsafe { slice::from_raw_parts(value.iov_base as *const u8, value.iov_len) }
}

/// An empty MDBX value for out-parameters.
const fn empty() -> ffi::MDBX_val {
    ffi::MDBX_val { iov_base: ptr::null_mut(), iov_len: 0 }
}

impl<K> Txn<K> {
    fn begin(env: &Env, flags: ffi::MDBX_txn_flags_t) -> Result<Self, MdbxError> {
        let mut txn = ptr::null_mut();
        // SAFETY: valid env handle; the out-pointer receives the transaction.
        MdbxError::check(unsafe {
            ffi::mdbx_txn_begin_ex(env.inner.ptr, ptr::null_mut(), flags, &mut txn, ptr::null_mut())
        })?;
        Ok(Self { ptr: txn, env: env.clone(), kind: PhantomData })
    }

    /// The transaction id (the snapshot a reader sees, or the id a writer will commit).
    pub fn id(&self) -> u64 {
        // SAFETY: valid transaction handle.
        unsafe { ffi::mdbx_txn_id(self.ptr) }
    }

    /// Opens an existing table by name, or `None` when the file has no such table.
    pub fn open_table(&self, name: &str) -> Result<Option<Dbi>, MdbxError> {
        let name = CString::new(name).expect("table names hold no NUL");
        let mut dbi = 0;
        // SAFETY: valid transaction; flags 0 opens without creating.
        let code = unsafe { ffi::mdbx_dbi_open(self.ptr, name.as_ptr(), ffi::MDBX_DB_ACCEDE, &mut dbi) };
        if code == ffi::MDBX_NOTFOUND {
            return Ok(None);
        }
        MdbxError::check(code)?;
        Ok(Some(Dbi(dbi)))
    }

    /// Reads the value of `key` (the first duplicate in a `DUPSORT` table).
    pub fn get(&self, dbi: Dbi, key: &[u8]) -> Result<Option<&[u8]>, MdbxError> {
        let key = val(key);
        let mut data = empty();
        // SAFETY: valid handles; on success `data` points into the map, valid while `self` is
        // borrowed (writes need `&mut self`).
        let code = unsafe { ffi::mdbx_get(self.ptr, dbi.0, &key, &mut data) };
        if code == ffi::MDBX_NOTFOUND {
            return Ok(None);
        }
        MdbxError::check(code)?;
        // SAFETY: see above.
        Ok(Some(unsafe { bytes(&data) }))
    }

    /// Opens a cursor over a table.
    pub fn cursor(&self, dbi: Dbi) -> Result<Cursor<'_, K>, MdbxError> {
        let mut cursor = ptr::null_mut();
        // SAFETY: valid handles.
        MdbxError::check(unsafe { ffi::mdbx_cursor_open(self.ptr, dbi.0, &mut cursor) })?;
        Ok(Cursor { ptr: cursor, txn: PhantomData })
    }

    /// Number of entries in a table.
    pub fn entries(&self, dbi: Dbi) -> Result<u64, MdbxError> {
        // SAFETY: MDBX_stat is plain data.
        let mut stat: ffi::MDBX_stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid handles and a correctly sized out-pointer.
        MdbxError::check(unsafe { ffi::mdbx_dbi_stat(self.ptr, dbi.0, &mut stat, size_of::<ffi::MDBX_stat>()) })?;
        Ok(stat.ms_entries)
    }
}

impl Txn<Rw> {
    /// Opens a table by name, creating it with `flags` (`MDBX_DUPSORT` or 0) if missing.
    pub fn create_table(&self, name: &str, dupsort: bool) -> Result<Dbi, MdbxError> {
        let name = CString::new(name).expect("table names hold no NUL");
        let flags = ffi::MDBX_CREATE | if dupsort { ffi::MDBX_DUPSORT } else { 0 };
        let mut dbi = 0;
        // SAFETY: valid write transaction.
        MdbxError::check(unsafe { ffi::mdbx_dbi_open(self.ptr, name.as_ptr(), flags, &mut dbi) })?;
        Ok(Dbi(dbi))
    }

    /// Writes `key` = `value`. In a `DUPSORT` table this adds a duplicate.
    pub fn put(&mut self, dbi: Dbi, key: &[u8], value: &[u8]) -> Result<(), MdbxError> {
        let key = val(key);
        let mut data = val(value);
        // SAFETY: valid handles; MDBX copies the bytes.
        MdbxError::check(unsafe { ffi::mdbx_put(self.ptr, dbi.0, &key, &mut data, ffi::MDBX_UPSERT) })
    }

    /// Appends `key` = `value` where `key` is greater than every key in the table.
    pub fn append(&mut self, dbi: Dbi, key: &[u8], value: &[u8]) -> Result<(), MdbxError> {
        let key = val(key);
        let mut data = val(value);
        // SAFETY: valid handles; MDBX copies the bytes.
        MdbxError::check(unsafe { ffi::mdbx_put(self.ptr, dbi.0, &key, &mut data, ffi::MDBX_APPEND) })
    }

    /// Deletes `key` (every duplicate when `value` is `None`, else that duplicate). Returns
    /// whether anything was deleted.
    pub fn delete(&mut self, dbi: Dbi, key: &[u8], value: Option<&[u8]>) -> Result<bool, MdbxError> {
        let key = val(key);
        let data = value.map(val);
        let data_ptr = data.as_ref().map_or(ptr::null(), |d| d as *const _);
        // SAFETY: valid handles.
        let code = unsafe { ffi::mdbx_del(self.ptr, dbi.0, &key, data_ptr) };
        if code == ffi::MDBX_NOTFOUND {
            return Ok(false);
        }
        MdbxError::check(code)?;
        Ok(true)
    }

    /// Deletes every entry of a table, keeping the table.
    pub fn clear(&mut self, dbi: Dbi) -> Result<(), MdbxError> {
        // SAFETY: valid handles; `false` empties without deleting the table.
        MdbxError::check(unsafe { ffi::mdbx_drop(self.ptr, dbi.0, false) })
    }

    /// Opens a cursor that can also write.
    pub fn cursor_rw(&mut self, dbi: Dbi) -> Result<Cursor<'_, Rw>, MdbxError> {
        let mut cursor = ptr::null_mut();
        // SAFETY: valid handles.
        MdbxError::check(unsafe { ffi::mdbx_cursor_open(self.ptr, dbi.0, &mut cursor) })?;
        Ok(Cursor { ptr: cursor, txn: PhantomData })
    }

    /// Commits, durably (fsync) before returning.
    pub fn commit(mut self) -> Result<(), MdbxError> {
        let txn = std::mem::replace(&mut self.ptr, ptr::null_mut());
        // SAFETY: valid transaction; MDBX frees it whether or not commit succeeds.
        MdbxError::check(unsafe { ffi::mdbx_txn_commit_ex(txn, ptr::null_mut()) })
    }
}

impl<K> Drop for Txn<K> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: an uncommitted transaction is aborted exactly once.
            unsafe { ffi::mdbx_txn_abort(self.ptr) };
        }
    }
}

/// A key-value pair borrowed from the map.
pub type Entry<'a> = (&'a [u8], &'a [u8]);

/// A cursor over one table, borrowed from its transaction.
#[derive(Debug)]
pub struct Cursor<'txn, K> {
    /// The MDBX cursor handle.
    pub ptr: *mut ffi::MDBX_cursor,
    /// Ties the cursor to its transaction.
    pub txn: PhantomData<&'txn Txn<K>>,
}

impl<K> Cursor<'_, K> {
    fn op(&mut self, key: Option<&[u8]>, value: Option<&[u8]>, op: ffi::MDBX_cursor_op) -> Result<Option<Entry<'_>>, MdbxError> {
        let mut k = key.map_or_else(empty, val);
        let mut v = value.map_or_else(empty, val);
        // SAFETY: valid cursor; results point into the map and are borrowed from `self`, so no
        // write through this cursor or its transaction happens while they live.
        let code = unsafe { ffi::mdbx_cursor_get(self.ptr, &mut k, &mut v, op) };
        if code == ffi::MDBX_NOTFOUND || code == ffi::MDBX_ENODATA {
            return Ok(None);
        }
        MdbxError::check(code)?;
        // SAFETY: see above.
        Ok(Some(unsafe { (bytes(&k), bytes(&v)) }))
    }

    /// Moves to the first entry.
    pub fn first(&mut self) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(None, None, ffi::MDBX_FIRST)
    }

    /// Moves to the last entry (the last duplicate of the last key).
    pub fn last(&mut self) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(None, None, ffi::MDBX_LAST)
    }

    /// Moves to the next entry (the next duplicate, or the first of the next key).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(None, None, ffi::MDBX_NEXT)
    }

    /// Moves to the previous entry.
    pub fn prev(&mut self) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(None, None, ffi::MDBX_PREV)
    }

    /// Moves to the first entry whose key is at least `key`.
    pub fn seek(&mut self, key: &[u8]) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(Some(key), None, ffi::MDBX_SET_RANGE)
    }

    /// Moves to `key` exactly (its first duplicate).
    pub fn seek_exact(&mut self, key: &[u8]) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(Some(key), None, ffi::MDBX_SET_KEY)
    }

    /// In a `DUPSORT` table: moves to the first duplicate of `key` whose value is at least
    /// `value`.
    pub fn seek_dup(&mut self, key: &[u8], value: &[u8]) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(Some(key), Some(value), ffi::MDBX_GET_BOTH_RANGE)
    }

    /// In a `DUPSORT` table: the next duplicate of the current key.
    pub fn next_dup(&mut self) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(None, None, ffi::MDBX_NEXT_DUP)
    }

    /// In a `DUPSORT` table: the first duplicate of the next key.
    pub fn next_key(&mut self) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(None, None, ffi::MDBX_NEXT_NODUP)
    }

    /// The current entry.
    pub fn current(&mut self) -> Result<Option<Entry<'_>>, MdbxError> {
        self.op(None, None, ffi::MDBX_GET_CURRENT)
    }
}

impl Cursor<'_, Rw> {
    /// Deletes the current entry (the current duplicate in a `DUPSORT` table).
    pub fn delete_current(&mut self) -> Result<(), MdbxError> {
        // SAFETY: valid cursor of a write transaction.
        MdbxError::check(unsafe { ffi::mdbx_cursor_del(self.ptr, ffi::MDBX_CURRENT) })
    }
}

impl<K> Drop for Cursor<'_, K> {
    fn drop(&mut self) {
        // SAFETY: the cursor is closed exactly once, before its transaction ends.
        unsafe { ffi::mdbx_cursor_close(self.ptr) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tiger-mdbx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plain_and_dupsort_tables_survive_reopen() {
        let dir = temp_dir("reopen");
        {
            let env = Env::open(&dir, EnvOptions::default()).unwrap();
            let mut txn = env.begin_rw().unwrap();
            let plain = txn.create_table("Plain", false).unwrap();
            let dups = txn.create_table("Dups", true).unwrap();
            txn.put(plain, b"b", b"2").unwrap();
            txn.put(plain, b"a", b"1").unwrap();
            txn.put(plain, b"a", b"one").unwrap();
            for value in [&b"z"[..], b"x", b"y"] {
                txn.put(dups, b"k", value).unwrap();
            }
            txn.commit().unwrap();
            // An aborted write is not visible.
            let mut txn = env.begin_rw().unwrap();
            txn.put(plain, b"c", b"3").unwrap();
            drop(txn);
        }
        let env = Env::open(&dir, EnvOptions::default()).unwrap();
        let txn = env.begin_ro().unwrap();
        let plain = txn.open_table("Plain").unwrap().unwrap();
        let dups = txn.open_table("Dups").unwrap().unwrap();
        assert!(txn.open_table("Missing").unwrap().is_none());
        assert_eq!(txn.get(plain, b"a").unwrap(), Some(&b"one"[..]));
        assert_eq!(txn.get(plain, b"c").unwrap(), None);
        let mut cursor = txn.cursor(plain).unwrap();
        assert_eq!(cursor.seek(b"aa").unwrap().map(|(k, _)| k.to_vec()), Some(b"b".to_vec()));
        let mut cursor = txn.cursor(dups).unwrap();
        assert_eq!(cursor.seek_dup(b"k", b"xx").unwrap().map(|(_, v)| v.to_vec()), Some(b"y".to_vec()));
        assert_eq!(cursor.next_dup().unwrap().map(|(_, v)| v.to_vec()), Some(b"z".to_vec()));
        assert_eq!(cursor.next_dup().unwrap(), None);
        assert_eq!(txn.entries(dups).unwrap(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
