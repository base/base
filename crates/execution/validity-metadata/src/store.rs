//! redb-backed durable store of per-block validity classification records.
//!
//! Records are keyed by exact block hash. Each successful [`ValidityMetadataStore::put_batch`]
//! is one atomic redb write transaction committed with immediate durability; an unclean
//! shutdown leaves the store at its last committed state. redb does not support concurrent
//! access from multiple processes, so a database file must be owned by one node process.
//!
//! Records are never pruned. Disk usage grows with the number of recorded blocks and the
//! operator is responsible for capacity and backups.

use std::{path::Path, sync::Arc};

use alloy_primitives::B256;
use redb::{Database, ReadableTable, TableDefinition};
use thiserror::Error;

use crate::{ValidityBlockRecord, ValidityRecordError};

/// Table mapping a block hash to its encoded [`ValidityBlockRecord`].
const RECORDS: TableDefinition<'_, &[u8; 32], &[u8]> = TableDefinition::new("validity_records");

/// Table of block hashes that received conflicting classifications.
const CONFLICTS: TableDefinition<'_, &[u8; 32], ()> = TableDefinition::new("validity_conflicts");

/// Errors returned by [`ValidityMetadataStore`].
#[derive(Debug, Error)]
pub enum ValidityMetadataError {
    /// The database could not be created, read or written.
    #[error("validity metadata storage error: {0}")]
    Storage(Box<redb::Error>),
    /// The database directory could not be created.
    #[error("failed to create validity metadata directory: {0}")]
    Io(#[from] std::io::Error),
    /// A stored record failed validation.
    #[error("corrupt validity metadata record for block {block_hash}: {reason}")]
    Corrupt {
        /// Key of the corrupt record.
        block_hash: B256,
        /// Validation failure.
        reason: ValidityRecordError,
    },
    /// The block received conflicting classifications, so no answer is trustworthy.
    #[error("conflicting validity metadata records for block {block_hash}")]
    Conflict {
        /// Block hash with conflicting records.
        block_hash: B256,
    },
    /// A record passed to [`ValidityMetadataStore::put_batch`] is invalid; nothing was written.
    #[error("invalid validity metadata record for block {block_hash}: {reason}")]
    InvalidRecord {
        /// Block hash of the invalid record.
        block_hash: B256,
        /// Validation failure.
        reason: ValidityRecordError,
    },
}

impl From<redb::Error> for ValidityMetadataError {
    fn from(error: redb::Error) -> Self {
        Self::Storage(Box::new(error))
    }
}

impl From<redb::DatabaseError> for ValidityMetadataError {
    fn from(error: redb::DatabaseError) -> Self {
        redb::Error::from(error).into()
    }
}

impl From<redb::TransactionError> for ValidityMetadataError {
    fn from(error: redb::TransactionError) -> Self {
        redb::Error::from(error).into()
    }
}

impl From<redb::TableError> for ValidityMetadataError {
    fn from(error: redb::TableError) -> Self {
        redb::Error::from(error).into()
    }
}

impl From<redb::StorageError> for ValidityMetadataError {
    fn from(error: redb::StorageError) -> Self {
        redb::Error::from(error).into()
    }
}

impl From<redb::CommitError> for ValidityMetadataError {
    fn from(error: redb::CommitError) -> Self {
        redb::Error::from(error).into()
    }
}

/// Per-record results of one [`ValidityMetadataStore::put_batch`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidityBatchOutcome {
    /// Records stored for the first time.
    pub inserted: usize,
    /// Records identical to an already stored record (idempotent retries).
    pub unchanged: usize,
    /// Records whose block hash now has, or already had, conflicting classifications.
    pub conflicting: usize,
}

/// Durable, cloneable handle to the validity metadata database.
///
/// All methods perform blocking disk I/O and must not run on an async executor thread.
#[derive(Debug, Clone)]
pub struct ValidityMetadataStore {
    db: Arc<Database>,
}

impl ValidityMetadataStore {
    /// Opens or creates the database at `path`, creating its parent directory if needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ValidityMetadataError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_database(Database::create(path)?)
    }

    /// Wraps an opened database, creating the tables if they do not exist.
    pub fn from_database(db: Database) -> Result<Self, ValidityMetadataError> {
        let txn = db.begin_write()?;
        txn.open_table(RECORDS)?;
        txn.open_table(CONFLICTS)?;
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Returns the validated record for `block_hash`, or `None` if the block was not recorded.
    ///
    /// Returns an error, never a record, when the block has conflicting classifications or the
    /// stored bytes fail validation.
    pub fn get(
        &self,
        block_hash: B256,
    ) -> Result<Option<ValidityBlockRecord>, ValidityMetadataError> {
        let txn = self.db.begin_read()?;
        if txn.open_table(CONFLICTS)?.get(&block_hash.0)?.is_some() {
            return Err(ValidityMetadataError::Conflict { block_hash });
        }
        let records = txn.open_table(RECORDS)?;
        let Some(value) = records.get(&block_hash.0)? else {
            return Ok(None);
        };
        let record = ValidityBlockRecord::decode(value.value())
            .map_err(|reason| ValidityMetadataError::Corrupt { block_hash, reason })?;
        if record.block_hash != block_hash {
            return Err(ValidityMetadataError::Corrupt {
                block_hash,
                reason: ValidityRecordError::KeyMismatch,
            });
        }
        Ok(Some(record))
    }

    /// Atomically stores `records` in one durable write transaction.
    ///
    /// Every record is validated before anything is written; one invalid record fails the
    /// whole batch with [`ValidityMetadataError::InvalidRecord`]. Storing a record identical to
    /// the stored one is a no-op. A different record for an already stored block hash never
    /// overwrites it: the block hash is durably marked as conflicting and [`Self::get`] returns
    /// [`ValidityMetadataError::Conflict`] for it from then on.
    pub fn put_batch(
        &self,
        records: &[ValidityBlockRecord],
    ) -> Result<ValidityBatchOutcome, ValidityMetadataError> {
        let encoded = records
            .iter()
            .map(|record| {
                record.encode().map(|bytes| (record.block_hash, bytes)).map_err(|reason| {
                    ValidityMetadataError::InvalidRecord { block_hash: record.block_hash, reason }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut outcome = ValidityBatchOutcome::default();
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(RECORDS)?;
            let mut conflicts = txn.open_table(CONFLICTS)?;
            for (block_hash, bytes) in &encoded {
                if conflicts.get(&block_hash.0)?.is_some() {
                    outcome.conflicting += 1;
                    continue;
                }
                let identical =
                    table.get(&block_hash.0)?.map(|existing| existing.value() == bytes.as_slice());
                match identical {
                    None => {
                        table.insert(&block_hash.0, bytes.as_slice())?;
                        outcome.inserted += 1;
                    }
                    Some(true) => outcome.unchanged += 1,
                    Some(false) => {
                        conflicts.insert(&block_hash.0, ())?;
                        outcome.conflicting += 1;
                    }
                }
            }
        }
        txn.commit()?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use alloy_primitives::b256;

    use super::*;
    use crate::test_utils::FailingBackend;

    const BLOCK: B256 = b256!("1111111111111111111111111111111111111111111111111111111111111111");
    const OTHER_BLOCK: B256 =
        b256!("2222222222222222222222222222222222222222222222222222222222222222");
    const TX_A: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const TX_B: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    fn open_temp() -> (tempfile::TempDir, ValidityMetadataStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ValidityMetadataStore::open(dir.path().join("nested/validity.redb")).unwrap();
        (dir, store)
    }

    /// Overwrites the raw stored bytes for `block_hash`, bypassing validation.
    fn write_raw(store: &ValidityMetadataStore, block_hash: B256, bytes: &[u8]) {
        let txn = store.db.begin_write().unwrap();
        txn.open_table(RECORDS).unwrap().insert(&block_hash.0, bytes).unwrap();
        txn.commit().unwrap();
    }

    #[test]
    fn missing_block_returns_none() {
        let (_dir, store) = open_temp();
        assert!(store.get(BLOCK).unwrap().is_none());
    }

    #[test]
    fn records_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("validity.redb");
        let with_validity = ValidityBlockRecord::new(BLOCK, 10, 3, vec![TX_A]);
        let empty = ValidityBlockRecord::new(OTHER_BLOCK, 11, 2, vec![]);
        {
            let store = ValidityMetadataStore::open(&path).unwrap();
            let outcome = store.put_batch(&[with_validity.clone(), empty.clone()]).unwrap();
            assert_eq!(outcome, ValidityBatchOutcome { inserted: 2, unchanged: 0, conflicting: 0 });
        }
        let store = ValidityMetadataStore::open(&path).unwrap();
        let reopened = store.get(BLOCK).unwrap().unwrap();
        assert_eq!(reopened, with_validity);
        assert!(reopened.contains(&TX_A));
        let reopened_empty = store.get(OTHER_BLOCK).unwrap().unwrap();
        assert_eq!(reopened_empty, empty);
        assert!(!reopened_empty.contains(&TX_A));
    }

    #[test]
    fn identical_retry_is_idempotent() {
        let (_dir, store) = open_temp();
        let record = ValidityBlockRecord::new(BLOCK, 10, 3, vec![TX_B, TX_A]);
        store.put_batch(std::slice::from_ref(&record)).unwrap();
        let retry = ValidityBlockRecord::new(BLOCK, 10, 3, vec![TX_A, TX_B, TX_A]);
        let outcome = store.put_batch(&[retry.clone(), retry]).unwrap();
        assert_eq!(outcome, ValidityBatchOutcome { inserted: 0, unchanged: 2, conflicting: 0 });
        assert_eq!(store.get(BLOCK).unwrap().unwrap(), record);
    }

    #[test]
    fn conflicting_record_is_never_answered_and_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("validity.redb");
        {
            let store = ValidityMetadataStore::open(&path).unwrap();
            store.put_batch(&[ValidityBlockRecord::new(BLOCK, 10, 3, vec![TX_A])]).unwrap();
            let outcome =
                store.put_batch(&[ValidityBlockRecord::new(BLOCK, 10, 3, vec![])]).unwrap();
            assert_eq!(outcome.conflicting, 1);
            assert!(matches!(
                store.get(BLOCK),
                Err(ValidityMetadataError::Conflict { block_hash }) if block_hash == BLOCK
            ));
            // Re-submitting the original classification does not clear the conflict.
            let outcome =
                store.put_batch(&[ValidityBlockRecord::new(BLOCK, 10, 3, vec![TX_A])]).unwrap();
            assert_eq!(outcome, ValidityBatchOutcome { inserted: 0, unchanged: 0, conflicting: 1 });
        }
        let store = ValidityMetadataStore::open(&path).unwrap();
        assert!(matches!(store.get(BLOCK), Err(ValidityMetadataError::Conflict { .. })));
    }

    #[test]
    fn conflict_within_one_batch_is_detected() {
        let (_dir, store) = open_temp();
        let outcome = store
            .put_batch(&[
                ValidityBlockRecord::new(BLOCK, 10, 3, vec![TX_A]),
                ValidityBlockRecord::new(BLOCK, 11, 3, vec![TX_A]),
                ValidityBlockRecord::new(OTHER_BLOCK, 12, 1, vec![]),
            ])
            .unwrap();
        assert_eq!(outcome, ValidityBatchOutcome { inserted: 2, unchanged: 0, conflicting: 1 });
        assert!(matches!(store.get(BLOCK), Err(ValidityMetadataError::Conflict { .. })));
        assert!(store.get(OTHER_BLOCK).unwrap().is_some());
    }

    #[test]
    fn invalid_record_rejects_whole_batch() {
        let (_dir, store) = open_temp();
        let valid = ValidityBlockRecord::new(OTHER_BLOCK, 12, 1, vec![]);
        let invalid = ValidityBlockRecord::new(BLOCK, 10, 0, vec![TX_A]);
        let error = store.put_batch(&[valid, invalid]).unwrap_err();
        assert!(matches!(
            error,
            ValidityMetadataError::InvalidRecord {
                block_hash,
                reason: ValidityRecordError::ExceedsTransactionCount { .. },
            } if block_hash == BLOCK
        ));
        assert!(store.get(OTHER_BLOCK).unwrap().is_none());
    }

    #[test]
    fn corrupt_stored_bytes_return_errors_not_records() {
        let (_dir, store) = open_temp();
        let valid = ValidityBlockRecord::new(BLOCK, 10, 3, vec![TX_A]).encode().unwrap();

        let mut unknown_version = valid.clone();
        unknown_version[0] = 9;
        let mut bad_count = valid.clone();
        bad_count[48] = 2;
        let cases: [(&[u8], ValidityRecordError); 4] = [
            (&valid[..20], ValidityRecordError::Truncated { len: 20 }),
            (&unknown_version, ValidityRecordError::UnsupportedVersion { version: 9 }),
            (&bad_count, ValidityRecordError::LengthMismatch { expected: 113, actual: 81 }),
            (
                &valid[..valid.len() - 1],
                ValidityRecordError::LengthMismatch { expected: 81, actual: 80 },
            ),
        ];
        for (bytes, expected) in cases {
            write_raw(&store, BLOCK, bytes);
            match store.get(BLOCK) {
                Err(ValidityMetadataError::Corrupt { block_hash, reason }) => {
                    assert_eq!(block_hash, BLOCK);
                    assert_eq!(reason, expected);
                }
                other => panic!("expected corrupt record error, got {other:?}"),
            }
        }
    }

    #[test]
    fn record_stored_under_wrong_key_is_corrupt() {
        let (_dir, store) = open_temp();
        let other = ValidityBlockRecord::new(OTHER_BLOCK, 10, 3, vec![]).encode().unwrap();
        write_raw(&store, BLOCK, &other);
        assert!(matches!(
            store.get(BLOCK),
            Err(ValidityMetadataError::Corrupt { reason: ValidityRecordError::KeyMismatch, .. })
        ));
    }

    #[test]
    fn corrupt_stored_bytes_conflict_with_new_record() {
        let (_dir, store) = open_temp();
        write_raw(&store, BLOCK, &[1, 2, 3]);
        let outcome = store.put_batch(&[ValidityBlockRecord::new(BLOCK, 10, 3, vec![])]).unwrap();
        assert_eq!(outcome.conflicting, 1);
        assert!(matches!(store.get(BLOCK), Err(ValidityMetadataError::Conflict { .. })));
    }

    #[test]
    fn storage_write_failure_is_an_error_and_not_a_record() {
        let (store, fail) = FailingBackend::store();

        fail.store(true, Ordering::SeqCst);
        let error = store.put_batch(&[ValidityBlockRecord::new(BLOCK, 10, 3, vec![])]).unwrap_err();
        assert!(matches!(error, ValidityMetadataError::Storage(_)));
        fail.store(false, Ordering::SeqCst);
        assert!(!matches!(store.get(BLOCK), Ok(Some(_))));
    }
}
