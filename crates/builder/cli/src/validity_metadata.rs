//! Builder-local validity metadata store and recorder opened from CLI configuration.

use std::path::Path;

use base_execution_validity_metadata::{
    DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY, ValidityMetadataRecorder, ValidityMetadataStore,
};
use tracing::{error, info, warn};

/// Validity metadata store and recorder shared by the native payload builder and the
/// `base_getValidityTransactionStatus` RPC within one process.
///
/// The default value is disabled: the recorder discards records and the RPC reports every
/// canonical transaction as not recorded.
#[derive(Debug, Clone, Default)]
pub struct ValidityMetadataService {
    /// Opened store, read by the status RPC. `None` when metadata is disabled or the store
    /// could not be opened.
    pub store: Option<ValidityMetadataStore>,
    /// Recorder handed to the native payload builder; disabled unless the writer thread started.
    pub recorder: ValidityMetadataRecorder,
}

impl ValidityMetadataService {
    /// Opens the store at `path` and starts its writer thread; `None` disables metadata.
    ///
    /// Metadata is optional for block production, so failures never stop the node: if the store
    /// cannot be opened, metadata is disabled; if the writer thread cannot start, previously
    /// recorded blocks stay readable but new blocks are not recorded. Both cases are logged as
    /// errors. Opening may block while redb checks or repairs the file after an unclean
    /// shutdown, so call this during startup, before block production begins.
    ///
    /// The writer thread is detached. It exits once every recorder clone is dropped; records
    /// still queued when the process exits are lost and report metadata as unavailable.
    pub fn open(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let store = match ValidityMetadataStore::open(path) {
            Ok(store) => store,
            Err(error) => {
                error!(
                    path = %path.display(),
                    error = %error,
                    "failed to open validity metadata store; validity metadata disabled"
                );
                return Self::default();
            }
        };
        match ValidityMetadataRecorder::spawn(
            store.clone(),
            DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY,
        ) {
            Ok((recorder, _writer)) => {
                info!(path = %path.display(), "validity metadata recording enabled");
                Self { store: Some(store), recorder }
            }
            Err(error) => {
                error!(
                    path = %path.display(),
                    error = %error,
                    "failed to start validity metadata writer; new blocks will not be recorded"
                );
                Self { store: Some(store), recorder: ValidityMetadataRecorder::default() }
            }
        }
    }

    /// Warns when configured node pruning hides recorded history from the status RPC.
    ///
    /// Lookups resolve hashes through the transaction-hash index and check block body indices;
    /// pruned transactions are reported as not canonically included (`null`), never as a
    /// classification.
    pub fn warn_on_pruned_history(&self, transaction_lookup_pruned: bool, bodies_pruned: bool) {
        if self.store.is_some() && (transaction_lookup_pruned || bodies_pruned) {
            warn!(
                transaction_lookup_pruned,
                bodies_pruned,
                "node pruning removes history needed by base_getValidityTransactionStatus; \
                 pruned transactions return null even when metadata was recorded"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_execution_validity_metadata::{ValidityBlockRecord, ValidityRecordStatus};

    use super::*;

    #[test]
    fn unset_path_disables_metadata() {
        let service = ValidityMetadataService::open(None);
        assert!(service.store.is_none());
        assert!(!service.recorder.is_enabled());
    }

    #[test]
    fn configured_path_shares_store_with_recorder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("validity.redb");
        let service = ValidityMetadataService::open(Some(&path));
        let record = ValidityBlockRecord {
            block_hash: B256::repeat_byte(1),
            block_number: 1,
            transaction_count: 1,
            validity_transactions: Vec::new(),
        };
        assert_eq!(service.recorder.record(record.clone()), ValidityRecordStatus::Queued);

        let store = service.store.expect("store must open");
        drop(service.recorder);
        // The writer thread exits after the last recorder drops; poll until the record lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while store.get(record.block_hash).unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "record was not written");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(store.get(record.block_hash).unwrap(), Some(record));
    }

    #[test]
    fn unopenable_store_disables_metadata_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        // A directory cannot be opened as a database file.
        let service = ValidityMetadataService::open(Some(dir.path()));
        assert!(service.store.is_none());
        assert!(!service.recorder.is_enabled());
    }
}
