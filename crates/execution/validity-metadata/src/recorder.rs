//! Bounded, non-blocking handoff of block records to a dedicated storage writer thread.
//!
//! [`ValidityMetadataRecorder::record`] only attempts a bounded channel send: it never performs
//! disk I/O, never waits for queue space and never reports failure through anything other than
//! its return value, metrics and rate-limited logs. A dedicated OS thread drains the queue into
//! atomic durable batches. Records still queued when the process stops, records dropped on a full
//! or closed queue and records in a failed batch are lost; they are never retried.

use std::{
    io,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread::JoinHandle,
};

use tracing::{debug, error, warn};

use crate::{ValidityBlockRecord, ValidityMetadataMetrics, ValidityMetadataStore};

/// Default capacity of the record queue between the block builder and the writer thread.
pub const DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// Maximum number of records committed in one write transaction.
pub const MAX_VALIDITY_METADATA_WRITE_BATCH: usize = 64;

/// Name of the dedicated writer thread.
pub const VALIDITY_METADATA_WRITER_THREAD: &str = "validity-metadata-writer";

/// Result of handing a record to [`ValidityMetadataRecorder::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidityRecordStatus {
    /// The record was queued for the writer thread.
    Queued,
    /// Metadata recording is disabled; the record was discarded.
    Disabled,
    /// The queue was full; the record was dropped.
    QueueFull,
    /// The writer thread has stopped; the record was dropped.
    Closed,
}

/// Cloneable producer handle for validity block records.
///
/// The default recorder is disabled and discards every record without side effects. The writer
/// thread exits once every clone of the recorder returned by [`Self::spawn`] is dropped and the
/// queue is drained.
#[derive(Debug, Clone, Default)]
pub struct ValidityMetadataRecorder {
    sender: Option<SyncSender<ValidityBlockRecord>>,
    dropped: Arc<AtomicU64>,
}

impl ValidityMetadataRecorder {
    /// Spawns the writer thread for `store` with a queue holding at most `capacity` records.
    ///
    /// Joining the returned handle waits for the writer to drain the queue after every recorder
    /// clone is dropped; detaching it is allowed.
    pub fn spawn(
        store: ValidityMetadataStore,
        capacity: NonZeroUsize,
    ) -> io::Result<(Self, JoinHandle<()>)> {
        let (sender, receiver) = sync_channel(capacity.get());
        let handle = std::thread::Builder::new()
            .name(VALIDITY_METADATA_WRITER_THREAD.to_string())
            .spawn(move || Self::run_writer(&store, &receiver))?;
        Ok((Self { sender: Some(sender), dropped: Arc::default() }, handle))
    }

    /// Returns whether records are forwarded to a writer thread.
    pub const fn is_enabled(&self) -> bool {
        self.sender.is_some()
    }

    /// Queues `record` without blocking; drops it if the queue is full or the writer stopped.
    pub fn record(&self, record: ValidityBlockRecord) -> ValidityRecordStatus {
        let Some(sender) = &self.sender else {
            return ValidityRecordStatus::Disabled;
        };
        match sender.try_send(record) {
            Ok(()) => {
                ValidityMetadataMetrics::records_queued().increment(1);
                ValidityRecordStatus::Queued
            }
            Err(TrySendError::Full(record)) => {
                self.note_dropped("queue_full", &record);
                ValidityRecordStatus::QueueFull
            }
            Err(TrySendError::Disconnected(record)) => {
                self.note_dropped("closed", &record);
                ValidityRecordStatus::Closed
            }
        }
    }

    /// Counts a dropped record and logs on the first drop and every power-of-two drop after it.
    fn note_dropped(&self, reason: &'static str, record: &ValidityBlockRecord) {
        ValidityMetadataMetrics::records_dropped(reason).increment(1);
        let dropped_total = self.dropped.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        if dropped_total.is_power_of_two() {
            warn!(
                reason,
                block = %record.block_hash,
                block_number = record.block_number,
                dropped_total,
                "dropped validity metadata record"
            );
        }
    }

    /// Drains the queue into batched writes until every sender is dropped.
    fn run_writer(store: &ValidityMetadataStore, receiver: &Receiver<ValidityBlockRecord>) {
        let mut batch = Vec::with_capacity(MAX_VALIDITY_METADATA_WRITE_BATCH);
        let mut failed_batches = 0u64;
        while let Ok(record) = receiver.recv() {
            batch.push(record);
            while batch.len() < MAX_VALIDITY_METADATA_WRITE_BATCH {
                let Ok(record) = receiver.try_recv() else { break };
                batch.push(record);
            }
            Self::write_batch(store, &mut batch, &mut failed_batches);
        }
        debug!("validity metadata writer stopped");
    }

    /// Normalizes, validates and commits `batch` once, leaving it empty.
    fn write_batch(
        store: &ValidityMetadataStore,
        batch: &mut Vec<ValidityBlockRecord>,
        failed_batches: &mut u64,
    ) {
        let mut valid = Vec::with_capacity(batch.len());
        for record in batch.drain(..) {
            let record = ValidityBlockRecord::new(
                record.block_hash,
                record.block_number,
                record.transaction_count,
                record.validity_transactions,
            );
            match record.validate() {
                Ok(()) => valid.push(record),
                Err(error) => {
                    ValidityMetadataMetrics::records_dropped("invalid").increment(1);
                    warn!(
                        block = %record.block_hash,
                        block_number = record.block_number,
                        error = %error,
                        "dropped invalid validity metadata record"
                    );
                }
            }
        }
        if valid.is_empty() {
            return;
        }
        match store.put_batch(&valid) {
            Ok(outcome) => {
                ValidityMetadataMetrics::records_written("inserted")
                    .increment(outcome.inserted as u64);
                ValidityMetadataMetrics::records_written("unchanged")
                    .increment(outcome.unchanged as u64);
                if outcome.conflicting > 0 {
                    ValidityMetadataMetrics::records_conflicting()
                        .increment(outcome.conflicting as u64);
                    warn!(
                        conflicting = outcome.conflicting,
                        "validity metadata records conflict with stored classifications"
                    );
                }
            }
            Err(err) => {
                ValidityMetadataMetrics::records_dropped("write_failed")
                    .increment(valid.len() as u64);
                *failed_batches = failed_batches.saturating_add(1);
                if failed_batches.is_power_of_two() {
                    error!(
                        error = %err,
                        records = valid.len(),
                        failed_batches = *failed_batches,
                        "failed to write validity metadata batch"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use alloy_primitives::{B256, TxHash, b256};
    use metrics::{SharedString, Unit};
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };

    use super::*;
    use crate::{
        MAX_VALIDITY_TRANSACTIONS_PER_RECORD, ValidityMetadataError, test_utils::FailingBackend,
    };

    const TX_A: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const TX_B: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    fn block(n: u8) -> B256 {
        B256::repeat_byte(n)
    }

    fn record(n: u8, validity: Vec<TxHash>) -> ValidityBlockRecord {
        ValidityBlockRecord {
            block_hash: block(n),
            block_number: u64::from(n),
            transaction_count: 4,
            validity_transactions: validity,
        }
    }

    type Snapshot = Vec<(CompositeKey, Option<Unit>, Option<SharedString>, DebugValue)>;

    /// Reads one counter from a snapshot; taking a snapshot drains counter values.
    fn counter(snapshot: &Snapshot, name: &str, label: Option<(&str, &str)>) -> u64 {
        snapshot
            .iter()
            .find_map(|(key, _, _, value)| {
                let matches = key.kind() == MetricKind::Counter
                    && key.key().name() == name
                    && label.is_none_or(|(k, v)| {
                        key.key().labels().any(|l| l.key() == k && l.value() == v)
                    });
                match value {
                    DebugValue::Counter(count) if matches => Some(*count),
                    _ => None,
                }
            })
            .unwrap_or(0)
    }

    #[test]
    fn default_recorder_is_disabled_and_discards() {
        let recorder = ValidityMetadataRecorder::default();
        assert!(!recorder.is_enabled());
        assert_eq!(recorder.record(record(1, vec![])), ValidityRecordStatus::Disabled);
    }

    #[test]
    fn full_and_closed_queues_drop_without_blocking() {
        let metrics = DebuggingRecorder::new();
        let snapshotter = metrics.snapshotter();
        let _guard = metrics::set_default_local_recorder(&metrics);

        // No reader drains this queue, so the second send can only succeed by blocking.
        let (sender, receiver) = sync_channel(1);
        let recorder = ValidityMetadataRecorder { sender: Some(sender), dropped: Arc::default() };
        let clone = recorder.clone();
        assert_eq!(recorder.record(record(1, vec![])), ValidityRecordStatus::Queued);
        assert_eq!(clone.record(record(2, vec![])), ValidityRecordStatus::QueueFull);
        assert_eq!(recorder.record(record(3, vec![])), ValidityRecordStatus::QueueFull);
        assert_eq!(receiver.try_recv().unwrap().block_hash, block(1));
        assert!(receiver.try_recv().is_err());

        drop(receiver);
        assert_eq!(recorder.record(record(4, vec![])), ValidityRecordStatus::Closed);
        assert_eq!(recorder.dropped.load(Ordering::Relaxed), 3);

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(counter(&snapshot, "builder.validity_metadata.records_queued", None), 1);
        let dropped = "builder.validity_metadata.records_dropped";
        assert_eq!(counter(&snapshot, dropped, Some(("reason", "queue_full"))), 2);
        assert_eq!(counter(&snapshot, dropped, Some(("reason", "closed"))), 1);
    }

    #[test]
    fn writer_persists_normalized_records_and_exits_when_recorders_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("validity.redb");
        let store = ValidityMetadataStore::open(&path).unwrap();
        let (recorder, handle) =
            ValidityMetadataRecorder::spawn(store.clone(), NonZeroUsize::new(8).unwrap()).unwrap();
        let clone = recorder.clone();

        assert_eq!(recorder.record(record(1, vec![TX_B, TX_A])), ValidityRecordStatus::Queued);
        assert_eq!(clone.record(record(2, vec![])), ValidityRecordStatus::Queued);
        // More validity transactions than block transactions: invalid, dropped by the writer.
        let mut invalid = record(3, vec![TX_A, TX_B]);
        invalid.transaction_count = 1;
        assert_eq!(clone.record(invalid), ValidityRecordStatus::Queued);

        drop(recorder);
        drop(clone);
        handle.join().unwrap();
        drop(store);

        let reopened = ValidityMetadataStore::open(&path).unwrap();
        let first = reopened.get(block(1)).unwrap().unwrap();
        assert_eq!(first.validity_transactions, vec![TX_A, TX_B]);
        assert!(first.contains(&TX_B));
        assert!(!reopened.get(block(2)).unwrap().unwrap().contains(&TX_A));
        assert!(reopened.get(block(3)).unwrap().is_none());
    }

    #[test]
    fn writer_stores_at_limit_record_and_drops_over_limit_record() {
        let metrics = DebuggingRecorder::new();
        let snapshotter = metrics.snapshotter();
        let _guard = metrics::set_default_local_recorder(&metrics);

        let hashes = |count: usize| -> Vec<TxHash> {
            (0..count as u64).map(|i| B256::left_padding_from(&i.to_be_bytes())).collect()
        };
        let mut at_limit = record(1, hashes(MAX_VALIDITY_TRANSACTIONS_PER_RECORD));
        at_limit.transaction_count = u32::MAX;
        let mut over_limit = record(2, hashes(MAX_VALIDITY_TRANSACTIONS_PER_RECORD + 1));
        over_limit.transaction_count = u32::MAX;

        let (store, _fail) = FailingBackend::store();
        let mut batch = vec![at_limit, over_limit];
        ValidityMetadataRecorder::write_batch(&store, &mut batch, &mut 0);

        let stored = store.get(block(1)).unwrap().unwrap();
        assert_eq!(stored.validity_transactions.len(), MAX_VALIDITY_TRANSACTIONS_PER_RECORD);
        assert!(store.get(block(2)).unwrap().is_none());
        let snapshot = snapshotter.snapshot().into_vec();
        let dropped = "builder.validity_metadata.records_dropped";
        assert_eq!(counter(&snapshot, dropped, Some(("reason", "invalid"))), 1);
        let written = "builder.validity_metadata.records_written";
        assert_eq!(counter(&snapshot, written, Some(("outcome", "inserted"))), 1);
    }

    #[test]
    fn writer_marks_conflicts_instead_of_overwriting() {
        let (store, _fail) = FailingBackend::store();
        let (recorder, handle) =
            ValidityMetadataRecorder::spawn(store.clone(), NonZeroUsize::new(4).unwrap()).unwrap();
        recorder.record(record(1, vec![TX_A]));
        recorder.record(record(1, vec![]));
        drop(recorder);
        handle.join().unwrap();
        assert!(matches!(store.get(block(1)), Err(ValidityMetadataError::Conflict { .. })));
    }

    #[test]
    fn failed_write_drops_batch_once_and_counts_it() {
        let metrics = DebuggingRecorder::new();
        let snapshotter = metrics.snapshotter();
        let _guard = metrics::set_default_local_recorder(&metrics);

        let (store, fail) = FailingBackend::store();
        fail.store(true, Ordering::SeqCst);
        let mut batch = vec![record(1, vec![TX_A]), record(2, vec![])];
        let mut failed_batches = 0;
        ValidityMetadataRecorder::write_batch(&store, &mut batch, &mut failed_batches);

        assert!(batch.is_empty());
        assert_eq!(failed_batches, 1);
        assert_eq!(
            counter(
                &snapshotter.snapshot().into_vec(),
                "builder.validity_metadata.records_dropped",
                Some(("reason", "write_failed"))
            ),
            2
        );
        fail.store(false, Ordering::SeqCst);
        assert!(!matches!(store.get(block(1)), Ok(Some(_))));
    }

    #[test]
    fn writer_thread_survives_storage_failure_and_exits() {
        let (store, fail) = FailingBackend::store();
        fail.store(true, Ordering::SeqCst);
        let (recorder, handle) =
            ValidityMetadataRecorder::spawn(store.clone(), NonZeroUsize::new(4).unwrap()).unwrap();
        assert_eq!(recorder.record(record(1, vec![TX_A])), ValidityRecordStatus::Queued);
        assert_eq!(recorder.record(record(2, vec![])), ValidityRecordStatus::Queued);
        drop(recorder);
        handle.join().unwrap();
        fail.store(false, Ordering::SeqCst);
        assert!(!matches!(store.get(block(1)), Ok(Some(_))));
        assert!(!matches!(store.get(block(2)), Ok(Some(_))));
    }
}
