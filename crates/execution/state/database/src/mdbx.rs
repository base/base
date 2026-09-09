//! Helper functions for initializing and opening a database.

use std::path::Path;

pub use crate::native_mdbx::*;
use base_common_observability_tracing::tracing::{info, warn};
use eyre::Context;

pub use crate::implementation::mdbx::*;
use crate::{TableSet, Tables, is_database_empty};

/// Tables that have been removed from the schema but may still exist on disk from previous
/// versions. These will be dropped during database initialization.
const ORPHAN_TABLES: &[&str] = &["AccountsTrieChangeSets", "StoragesTrieChangeSets"];

/// Checks if the given path resides on a ZFS filesystem and logs a warning.
///
/// ZFS uses copy-on-write (COW) semantics which conflict with MDBX's write patterns, leading to
/// significant performance degradation.
fn warn_if_zfs(path: &Path) {
    if matches!(is_zfs(path), Ok(true)) {
        warn!(
            target: "reth::db",
            path = %path.display(),
            "Database is on a ZFS filesystem. ZFS's copy-on-write behavior causes significant \
             performance degradation with MDBX. Consider using ext4 or xfs instead."
        );
    }
}

/// Returns `true` if the given path is on a ZFS filesystem.
#[cfg(target_os = "linux")]
fn is_zfs(path: &Path) -> std::io::Result<bool> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    /// ZFS filesystem magic number.
    const ZFS_SUPER_MAGIC: i64 = 0x2fc12fc1;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    unsafe {
        let mut stat: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &raw mut stat) == 0 {
            Ok(stat.f_type == ZFS_SUPER_MAGIC)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// Returns `true` if the given path is on a ZFS filesystem.
#[cfg(target_os = "macos")]
fn is_zfs(path: &Path) -> std::io::Result<bool> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    unsafe {
        let mut stat: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &raw mut stat) == 0 {
            let fstype = std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr());
            Ok(fstype.to_bytes() == b"zfs")
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// ZFS detection is unsupported on this platform, always returns `Ok(false)`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_zfs(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

/// Creates a new database at the specified path if it doesn't exist. Does NOT create tables. Check
/// [`init_db`].
pub fn create_db<P: AsRef<Path>>(path: P, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    use crate::version::{DatabaseVersionError, check_db_version_file, create_db_version_file};

    let rpath = path.as_ref();
    warn_if_zfs(rpath);

    if is_database_empty(rpath) {
        base_common_io_files::Files::create_dir_all(rpath)
            .wrap_err_with(|| format!("Could not create database directory {}", rpath.display()))?;
        create_db_version_file(rpath)?;
    } else {
        match check_db_version_file(rpath) {
            Ok(_) => (),
            Err(DatabaseVersionError::MissingFile) => create_db_version_file(rpath)?,
            Err(err) => return Err(err.into()),
        }
    }

    Ok(DatabaseEnv::open(rpath, DatabaseEnvKind::RW, args)?)
}

/// Opens up an existing database or creates a new one at the specified path. Creates tables defined
/// in [`Tables`] if necessary. Read/Write mode.
pub fn init_db<P: AsRef<Path>>(path: P, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    init_db_for::<P, Tables>(path, args)
}

/// Opens up an existing database or creates a new one at the specified path. Creates tables defined
/// in the given [`TableSet`] if necessary. Read/Write mode.
pub fn init_db_for<P: AsRef<Path>, TS: TableSet>(
    path: P,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    let client_version = args.client_version().clone();
    let mut db = create_db(path, args)?;
    db.create_and_track_tables_for::<TS>()?;
    db.record_client_version(client_version)?;
    drop_orphan_tables(&db);
    Ok(db)
}

/// Drops orphaned tables that are no longer part of the schema.
fn drop_orphan_tables(db: &DatabaseEnv) {
    for table_name in ORPHAN_TABLES {
        match db.drop_orphan_table(table_name) {
            Ok(true) => {
                info!(target: "reth::db", table = %table_name, "Dropped orphaned database table");
            }
            Ok(false) => {}
            Err(e) => {
                base_common_observability_tracing::tracing::warn!(
                    target: "reth::db",
                    table = %table_name,
                    %e,
                    "Failed to drop orphaned database table"
                );
            }
        }
    }
}

/// Opens up an existing database. Read only mode. It doesn't create it or create tables if missing.
pub fn open_db_read_only(
    path: impl AsRef<Path>,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    let path = path.as_ref();
    DatabaseEnv::open(path, DatabaseEnvKind::RO, args)
        .with_context(|| format!("Could not open database at path: {}", path.display()))
}

/// Opens up an existing database. Read/Write mode with `WriteMap` enabled. It doesn't create it or
/// create tables if missing.
pub fn open_db(path: impl AsRef<Path>, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    fn open(path: &Path, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
        let client_version = args.client_version().clone();
        let db = DatabaseEnv::open(path, DatabaseEnvKind::RW, args)
            .with_context(|| format!("Could not open database at path: {}", path.display()))?;
        db.record_client_version(client_version)?;
        Ok(db)
    }
    open(path.as_ref(), args)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::mdbx::MaxReadTransactionDuration;
    use crate::{Database, DbCursorRO, DbTx, models::ClientVersion};
    use assert_matches::assert_matches;
    use tempfile::tempdir;

    use crate::{
        init_db,
        mdbx::DatabaseArguments,
        open_db, tables,
        version::{DatabaseVersionError, db_version_file_path},
    };

    #[test]
    fn test_temp_database_cleanup() {
        // Test that TempDatabase properly cleans up its directory when dropped
        let temp_path = {
            let db = crate::test_utils::create_test_rw_db();
            let path = db.path();
            assert!(path.exists(), "Database directory should exist while TempDatabase is alive");
            path
            // TempDatabase dropped here
        };

        // Verify the directory was cleaned up
        assert!(
            !temp_path.exists(),
            "Database directory should be cleaned up after TempDatabase is dropped"
        );
    }

    #[test]
    fn db_version() {
        let path = tempdir().unwrap();

        let args = DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded));

        // Database is empty
        {
            let db = init_db(&path, args.clone());
            assert_matches!(db, Ok(_));
        }

        // Database is not empty, current version is the same as in the file
        {
            let db = init_db(&path, args.clone());
            assert_matches!(db, Ok(_));
        }

        // Database is not empty, version file is malformed
        {
            base_common_io_files::Files::write(
                path.path().join(db_version_file_path(&path)),
                "invalid-version",
            )
            .unwrap();
            let db = init_db(&path, args.clone());
            assert!(db.is_err());
            assert_matches!(
                db.unwrap_err().downcast_ref::<DatabaseVersionError>(),
                Some(DatabaseVersionError::MalformedFile)
            )
        }

        // Database is not empty, version file contains not matching version
        {
            base_common_io_files::Files::write(path.path().join(db_version_file_path(&path)), "0")
                .unwrap();
            let db = init_db(&path, args);
            assert!(db.is_err());
            assert_matches!(
                db.unwrap_err().downcast_ref::<DatabaseVersionError>(),
                Some(DatabaseVersionError::VersionMismatch { version: 0 })
            )
        }
    }

    #[test]
    fn db_client_version() {
        let path = tempdir().unwrap();

        // Empty client version is not recorded
        {
            let db = init_db(&path, DatabaseArguments::new(ClientVersion::default())).unwrap();
            let tx = db.tx().unwrap();
            let mut cursor = tx.cursor_read::<tables::VersionHistory>().unwrap();
            assert_matches!(cursor.first(), Ok(None));
        }

        // Client version is recorded
        let first_version = ClientVersion { version: String::from("v1"), ..Default::default() };
        {
            let db = init_db(&path, DatabaseArguments::new(first_version.clone())).unwrap();
            let tx = db.tx().unwrap();
            let mut cursor = tx.cursor_read::<tables::VersionHistory>().unwrap();
            assert_eq!(
                cursor
                    .walk_range(..)
                    .unwrap()
                    .map(|x| x.map(|(_, v)| v))
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                vec![first_version.clone()]
            );
        }

        // Same client version is not duplicated.
        {
            let db = init_db(&path, DatabaseArguments::new(first_version.clone())).unwrap();
            let tx = db.tx().unwrap();
            let mut cursor = tx.cursor_read::<tables::VersionHistory>().unwrap();
            assert_eq!(
                cursor
                    .walk_range(..)
                    .unwrap()
                    .map(|x| x.map(|(_, v)| v))
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                vec![first_version.clone()]
            );
        }

        // Different client version is recorded
        std::thread::sleep(Duration::from_secs(1));
        let second_version = ClientVersion { version: String::from("v2"), ..Default::default() };
        {
            let db = init_db(&path, DatabaseArguments::new(second_version.clone())).unwrap();
            let tx = db.tx().unwrap();
            let mut cursor = tx.cursor_read::<tables::VersionHistory>().unwrap();
            assert_eq!(
                cursor
                    .walk_range(..)
                    .unwrap()
                    .map(|x| x.map(|(_, v)| v))
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                vec![first_version.clone(), second_version.clone()]
            );
        }

        // Different client version is recorded on db open.
        std::thread::sleep(Duration::from_secs(1));
        let third_version = ClientVersion { version: String::from("v3"), ..Default::default() };
        {
            let db = open_db(path.path(), DatabaseArguments::new(third_version.clone())).unwrap();
            let tx = db.tx().unwrap();
            let mut cursor = tx.cursor_read::<tables::VersionHistory>().unwrap();
            assert_eq!(
                cursor
                    .walk_range(..)
                    .unwrap()
                    .map(|x| x.map(|(_, v)| v))
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                vec![first_version, second_version, third_version]
            );
        }
    }
}
