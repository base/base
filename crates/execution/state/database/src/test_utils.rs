//! Temporary databases and database fixtures.

use std::{
    fmt::Formatter,
    path::{Path, PathBuf},
    sync::Arc,
};

use base_common_io;
use parking_lot::RwLock;
use tempfile::TempDir;

use super::*;
use crate::{Database, DatabaseMetrics, mdbx::DatabaseArguments};

/// Error during database open
pub const ERROR_DB_OPEN: &str = "could not open the database file";
/// Error during database creation
pub const ERROR_DB_CREATION: &str = "could not create the database file";
/// Error during database creation
pub const ERROR_STATIC_FILES_CREATION: &str = "could not create the static file path";
/// Error during table creation
pub const ERROR_TABLE_CREATION: &str = "could not create tables in the database";
/// Error during tempdir creation
pub const ERROR_TEMPDIR: &str = "could not create a temporary directory";

/// A database will delete the db dir when dropped.
pub struct TempDatabase<DB> {
    db: Option<DB>,
    path: PathBuf,
    /// Executed right before a database transaction is created.
    pub pre_tx_hook: RwLock<Box<dyn Fn() + Send + Sync>>,
    /// Executed right after a database transaction is created.
    pub post_tx_hook: RwLock<Box<dyn Fn() + Send + Sync>>,
}

impl<DB: std::fmt::Debug> std::fmt::Debug for TempDatabase<DB> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempDatabase").field("db", &self.db).field("path", &self.path).finish()
    }
}

impl<DB> Drop for TempDatabase<DB> {
    fn drop(&mut self) {
        if let Some(db) = self.db.take() {
            drop(db);
            let _ = base_common_io::Files::remove_dir_all(&self.path);
        }
    }
}

impl<DB> TempDatabase<DB> {
    /// Create new [`TempDatabase`] instance.
    pub fn new(db: DB, path: PathBuf) -> Self {
        Self {
            db: Some(db),
            path,
            pre_tx_hook: RwLock::new(Box::new(|| ())),
            post_tx_hook: RwLock::new(Box::new(|| ())),
        }
    }

    /// Returns the reference to inner db.
    pub const fn db(&self) -> &DB {
        self.db.as_ref().unwrap()
    }

    /// Returns the path to the database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Convert temp database into inner.
    pub fn into_inner_db(mut self) -> DB {
        self.db.take().unwrap() // take out db to avoid clean path in drop fn
    }

    /// Sets [`TempDatabase`] new pre transaction creation hook.
    pub fn set_pre_transaction_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        let mut db_hook = self.pre_tx_hook.write();
        *db_hook = hook;
    }

    /// Sets [`TempDatabase`] new post transaction creation hook.
    pub fn set_post_transaction_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        let mut db_hook = self.post_tx_hook.write();
        *db_hook = hook;
    }
}

impl<DB: Database> Database for TempDatabase<DB> {
    type TX = <DB as Database>::TX;
    type TXMut = <DB as Database>::TXMut;
    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        self.pre_tx_hook.read()();
        let tx = self.db().tx()?;
        self.post_tx_hook.read()();
        Ok(tx)
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        self.db().tx_mut()
    }

    fn path(&self) -> std::path::PathBuf {
        self.db().path()
    }

    fn oldest_reader_txnid(&self) -> Option<u64> {
        self.db().oldest_reader_txnid()
    }

    fn last_txnid(&self) -> Option<u64> {
        self.db().last_txnid()
    }
}

impl<DB: DatabaseMetrics> DatabaseMetrics for TempDatabase<DB> {
    fn report_metrics(&self) {
        self.db().report_metrics()
    }
}

/// Create `static_files` path for testing
#[track_caller]
pub fn create_test_static_files_dir() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::with_prefix("reth-test-static-").expect(ERROR_TEMPDIR);
    let path = temp_dir.path().to_path_buf();
    (temp_dir, path)
}

/// Create `rocksdb` path for testing
#[track_caller]
pub fn create_test_rocksdb_dir() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::with_prefix("reth-test-rocksdb-").expect(ERROR_TEMPDIR);
    let path = temp_dir.path().to_path_buf();
    (temp_dir, path)
}

/// Get a temporary directory path to use for the database
pub fn tempdir_path() -> PathBuf {
    let builder = tempfile::Builder::new().prefix("reth-test-").rand_bytes(8).tempdir();
    builder.expect(ERROR_TEMPDIR).keep()
}

/// Create read/write database for testing
#[track_caller]
pub fn create_test_rw_db() -> Arc<TempDatabase<DatabaseEnv>> {
    let path = tempdir_path();
    let emsg = format!("{ERROR_DB_CREATION}: {path:?}");

    let db = init_db(&path, DatabaseArguments::test()).expect(&emsg);

    Arc::new(TempDatabase::new(db, path))
}

/// Create read/write database for testing
#[track_caller]
pub fn create_test_rw_db_with_path<P: AsRef<Path>>(path: P) -> Arc<TempDatabase<DatabaseEnv>> {
    let path = path.as_ref().to_path_buf();
    let emsg = format!("{ERROR_DB_CREATION}: {path:?}");
    let db = init_db(path.as_path(), DatabaseArguments::test()).expect(&emsg);
    Arc::new(TempDatabase::new(db, path))
}

/// Create read/write database for testing within a data directory.
///
/// The database is created at `datadir/db`, and `TempDatabase` will clean up the entire
/// `datadir` on drop.
#[track_caller]
pub fn create_test_rw_db_with_datadir<P: AsRef<Path>>(
    datadir: P,
) -> Arc<TempDatabase<DatabaseEnv>> {
    let datadir = datadir.as_ref().to_path_buf();
    let db_path = datadir.join("db");
    let emsg = format!("{ERROR_DB_CREATION}: {db_path:?}");
    let db = init_db(&db_path, DatabaseArguments::test()).expect(&emsg);
    Arc::new(TempDatabase::new(db, datadir))
}

/// Create read only database for testing
#[track_caller]
pub fn create_test_ro_db() -> Arc<TempDatabase<DatabaseEnv>> {
    let args = DatabaseArguments::test();

    let path = tempdir_path();
    let emsg = format!("{ERROR_DB_CREATION}: {path:?}");
    {
        init_db(path.as_path(), args.clone()).expect(&emsg);
    }
    let db = open_db_read_only(path.as_path(), args).expect(ERROR_DB_OPEN);
    Arc::new(TempDatabase::new(db, path))
}

/// Enables MDBX legacy multi-open mode, allowing the same database to be opened
/// multiple times within a single process. This is needed for tests that simulate
/// concurrent primary + read-only secondary provider scenarios.
///
/// Must be called before any MDBX environment is opened.
///
/// # Safety
///
/// This uses `MDBX_DBG_LEGACY_MULTIOPEN` which recovers POSIX file locks on close.
/// It may cause unexpected pauses and does not perfectly mirror multi-process behavior.
/// Use only in tests.
pub fn enable_legacy_multiopen() {
    unsafe {
        crate::mdbx::ffi::mdbx_setup_debug(
            crate::mdbx::ffi::MDBX_LOG_DONTCHANGE,
            crate::mdbx::ffi::MDBX_DBG_LEGACY_MULTIOPEN as crate::mdbx::ffi::MDBX_debug_flags,
            None,
        );
    }
}
