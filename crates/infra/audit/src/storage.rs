use std::{fmt, fmt::Debug, time::Instant};

use alloy_primitives::TxHash;
use anyhow::Result;
use async_trait::async_trait;
use aws_sdk_s3::{
    Client as S3Client, error::SdkError, operation::get_object::GetObjectError,
    primitives::ByteStream,
};
use base_primitives::AcceptedBundle;
use futures::future;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    metrics::Metrics,
    reader::Event,
    types::{BundleEvent, BundleId, DropReason, TransactionId},
};

/// S3 key types for storing different event types.
#[derive(Debug)]
pub enum S3Key {
    /// Key for bundle events.
    Bundle(BundleId),
    /// Key for transaction lookups by hash.
    TransactionByHash(TxHash),
}

impl fmt::Display for S3Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundle(bundle_id) => write!(f, "bundles/{bundle_id}"),
            Self::TransactionByHash(hash) => write!(f, "transactions/by_hash/{hash}"),
        }
    }
}

/// Metadata for a transaction, tracking which bundles it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransactionMetadata {
    /// Bundle IDs that contain this transaction.
    pub bundle_ids: Vec<BundleId>,
}

/// History event for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum BundleHistoryEvent {
    /// Bundle was received.
    Received {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// The accepted bundle.
        bundle: Box<AcceptedBundle>,
    },
    /// Bundle was cancelled.
    Cancelled {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
    },
    /// Bundle was included by a builder.
    BuilderIncluded {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Builder identifier.
        builder: String,
        /// Block number.
        block_number: u64,
        /// Flashblock index.
        flashblock_index: u64,
    },
    /// Bundle was included in a block.
    BlockIncluded {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Block number.
        block_number: u64,
        /// Block hash.
        block_hash: TxHash,
    },
    /// Bundle was dropped.
    Dropped {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Drop reason.
        reason: DropReason,
    },
}

impl BundleHistoryEvent {
    /// Returns the event key.
    pub fn key(&self) -> &str {
        match self {
            Self::Received { key, .. }
            | Self::Cancelled { key, .. }
            | Self::BuilderIncluded { key, .. }
            | Self::BlockIncluded { key, .. }
            | Self::Dropped { key, .. } => key,
        }
    }
}

/// History of events for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BundleHistory {
    /// List of history events.
    pub history: Vec<BundleHistoryEvent>,
}

fn update_bundle_history_transform(
    bundle_history: BundleHistory,
    event: &Event,
) -> Option<BundleHistory> {
    let mut history = bundle_history.history;
    let bundle_id = event.event.bundle_id();

    // Check for deduplication - if event with same key already exists, skip
    if history.iter().any(|h| h.key() == event.key) {
        info!(
            bundle_id = %bundle_id,
            event_key = %event.key,
            "Event already exists, skipping due to deduplication"
        );
        return None;
    }

    let history_event = match &event.event {
        BundleEvent::Received { bundle, .. } => BundleHistoryEvent::Received {
            key: event.key.clone(),
            timestamp: event.timestamp,
            bundle: bundle.clone(),
        },
        BundleEvent::Cancelled { .. } => {
            BundleHistoryEvent::Cancelled { key: event.key.clone(), timestamp: event.timestamp }
        }
        BundleEvent::BuilderIncluded { builder, block_number, flashblock_index, .. } => {
            BundleHistoryEvent::BuilderIncluded {
                key: event.key.clone(),
                timestamp: event.timestamp,
                builder: builder.clone(),
                block_number: *block_number,
                flashblock_index: *flashblock_index,
            }
        }
        BundleEvent::BlockIncluded { block_number, block_hash, .. } => {
            BundleHistoryEvent::BlockIncluded {
                key: event.key.clone(),
                timestamp: event.timestamp,
                block_number: *block_number,
                block_hash: *block_hash,
            }
        }
        BundleEvent::Dropped { reason, .. } => BundleHistoryEvent::Dropped {
            key: event.key.clone(),
            timestamp: event.timestamp,
            reason: reason.clone(),
        },
    };

    history.push(history_event);
    let bundle_history = BundleHistory { history };

    info!(
        bundle_id = %bundle_id,
        event_count = bundle_history.history.len(),
        "Updated bundle history"
    );

    Some(bundle_history)
}

fn update_transaction_metadata_transform(
    transaction_metadata: TransactionMetadata,
    bundle_id: BundleId,
) -> Option<TransactionMetadata> {
    let mut bundle_ids = transaction_metadata.bundle_ids;

    if bundle_ids.contains(&bundle_id) {
        return None;
    }

    bundle_ids.push(bundle_id);
    Some(TransactionMetadata { bundle_ids })
}

/// Trait for writing bundle events to storage.
#[async_trait]
pub trait EventWriter {
    /// Archives a bundle event.
    async fn archive_event(&self, event: Event) -> Result<()>;
}

/// Trait for reading bundle events from S3.
#[async_trait]
pub trait BundleEventS3Reader {
    /// Gets the bundle history for a given bundle ID.
    async fn get_bundle_history(&self, bundle_id: BundleId) -> Result<Option<BundleHistory>>;
    /// Gets transaction metadata for a given transaction hash.
    async fn get_transaction_metadata(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<TransactionMetadata>>;
}

/// S3-backed event reader and writer.
#[derive(Clone, Debug)]
pub struct S3EventReaderWriter {
    s3_client: S3Client,
    bucket: String,
    metrics: Metrics,
}

impl S3EventReaderWriter {
    /// Creates a new S3 event reader/writer.
    pub fn new(s3_client: S3Client, bucket: String) -> Self {
        Self { s3_client, bucket, metrics: Metrics::default() }
    }

    async fn update_bundle_history(&self, event: Event) -> Result<()> {
        let s3_key = S3Key::Bundle(event.event.bundle_id()).to_string();

        self.idempotent_write::<BundleHistory, _>(&s3_key, |current_history| {
            update_bundle_history_transform(current_history, &event)
        })
        .await
    }

    async fn update_transaction_by_hash_index(
        &self,
        tx_id: &TransactionId,
        bundle_id: BundleId,
    ) -> Result<()> {
        let s3_key = S3Key::TransactionByHash(tx_id.hash);
        let key = s3_key.to_string();

        self.idempotent_write::<TransactionMetadata, _>(&key, |current_metadata| {
            update_transaction_metadata_transform(current_metadata, bundle_id)
        })
        .await
    }

    async fn idempotent_write<T, F>(&self, key: &str, mut transform_fn: F) -> Result<()>
    where
        T: for<'de> Deserialize<'de> + Serialize + Clone + Default + Debug,
        F: FnMut(T) -> Option<T>,
    {
        const MAX_RETRIES: usize = 5;
        const BASE_DELAY_MS: u64 = 100;

        for attempt in 0..MAX_RETRIES {
            let get_start = Instant::now();
            let (current_value, etag) = self.get_object_with_etag::<T>(key).await?;
            self.metrics.s3_get_duration.record(get_start.elapsed().as_secs_f64());

            let value = current_value.unwrap_or_default();

            match transform_fn(value.clone()) {
                Some(new_value) => {
                    let content = serde_json::to_string(&new_value)?;

                    let mut put_request = self
                        .s3_client
                        .put_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .body(ByteStream::from(content.into_bytes()));

                    if let Some(etag) = etag {
                        put_request = put_request.if_match(etag);
                    } else {
                        put_request = put_request.if_none_match("*");
                    }

                    let put_start = Instant::now();
                    match put_request.send().await {
                        Ok(_) => {
                            self.metrics.s3_put_duration.record(put_start.elapsed().as_secs_f64());
                            info!(
                                s3_key = %key,
                                attempt = attempt + 1,
                                "Successfully wrote object with idempotent write"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            self.metrics.s3_put_duration.record(put_start.elapsed().as_secs_f64());

                            if attempt < MAX_RETRIES - 1 {
                                let delay = BASE_DELAY_MS * 2_u64.pow(attempt as u32);
                                info!(
                                    s3_key = %key,
                                    attempt = attempt + 1,
                                    delay_ms = delay,
                                    error = %e,
                                    "Conflict detected, retrying with backoff"
                                );
                                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                            } else {
                                return Err(anyhow::anyhow!(
                                    "Failed to write after {MAX_RETRIES} attempts: {e}"
                                ));
                            }
                        }
                    }
                }
                None => {
                    self.metrics.s3_writes_skipped.increment(1);
                    info!(
                        s3_key = %key,
                        "Transform function returned None, no write required"
                    );
                    return Ok(());
                }
            }
        }

        Err(anyhow::anyhow!("Exceeded maximum retry attempts"))
    }

    async fn get_object_with_etag<T>(&self, key: &str) -> Result<(Option<T>, Option<String>)>
    where
        T: for<'de> Deserialize<'de>,
    {
        match self.s3_client.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(response) => {
                let etag = response.e_tag().map(|s| s.to_string());
                let body = response.body.collect().await?;
                let content = String::from_utf8(body.into_bytes().to_vec())?;
                let value: T = serde_json::from_str(&content)?;
                Ok((Some(value), etag))
            }
            Err(e) => match &e {
                SdkError::ServiceError(service_err) => match service_err.err() {
                    GetObjectError::NoSuchKey(_) => Ok((None, None)),
                    _ => Err(anyhow::anyhow!("Failed to get object: {e}")),
                },
                _ => {
                    let error_string = e.to_string();
                    if error_string.contains("NoSuchKey")
                        || error_string.contains("NotFound")
                        || error_string.contains("404")
                    {
                        Ok((None, None))
                    } else {
                        Err(anyhow::anyhow!("Failed to get object: {e}"))
                    }
                }
            },
        }
    }
}

#[async_trait]
impl EventWriter for S3EventReaderWriter {
    async fn archive_event(&self, event: Event) -> Result<()> {
        let bundle_id = event.event.bundle_id();
        let transaction_ids = event.event.transaction_ids();

        let bundle_start = Instant::now();
        let bundle_future = self.update_bundle_history(event);

        let tx_start = Instant::now();
        let tx_futures: Vec<_> =
            transaction_ids
                .into_iter()
                .map(|tx_id| async move {
                    self.update_transaction_by_hash_index(&tx_id, bundle_id).await
                })
                .collect();

        // Run the bundle and transaction futures concurrently and wait for them to complete
        tokio::try_join!(bundle_future, future::try_join_all(tx_futures))?;

        self.metrics.update_bundle_history_duration.record(bundle_start.elapsed().as_secs_f64());
        self.metrics.update_tx_indexes_duration.record(tx_start.elapsed().as_secs_f64());

        Ok(())
    }
}

#[async_trait]
impl BundleEventS3Reader for S3EventReaderWriter {
    async fn get_bundle_history(&self, bundle_id: BundleId) -> Result<Option<BundleHistory>> {
        let s3_key = S3Key::Bundle(bundle_id).to_string();
        let (bundle_history, _) = self.get_object_with_etag::<BundleHistory>(&s3_key).await?;
        Ok(bundle_history)
    }

    async fn get_transaction_metadata(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<TransactionMetadata>> {
        let s3_key = S3Key::TransactionByHash(tx_hash).to_string();
        let (transaction_metadata, _) =
            self.get_object_with_etag::<TransactionMetadata>(&s3_key).await?;
        Ok(transaction_metadata)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;
    use base_primitives::{BundleExtensions, bundles::test_utils::create_bundle_from_txn_data};
    use uuid::Uuid;

    use super::*;
    use crate::{
        reader::Event,
        types::{BundleEvent, DropReason},
    };

    fn create_test_event(key: &str, timestamp: i64, bundle_event: BundleEvent) -> Event {
        Event { key: key.to_string(), timestamp, event: bundle_event }
    }

    #[test]
    fn test_update_bundle_history_transform_adds_new_event() {
        let bundle_history = BundleHistory { history: vec![] };
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        let bundle_event = BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) };
        let event = create_test_event("test-key", 1234567890, bundle_event);

        let result = update_bundle_history_transform(bundle_history, &event);

        assert!(result.is_some());
        let bundle_history = result.unwrap();
        assert_eq!(bundle_history.history.len(), 1);

        match &bundle_history.history[0] {
            BundleHistoryEvent::Received { key, timestamp: ts, bundle: b } => {
                assert_eq!(key, "test-key");
                assert_eq!(*ts, 1234567890);
                assert_eq!(b.block_number, bundle.block_number);
            }
            _ => panic!("Expected Created event"),
        }
    }

    #[test]
    fn test_update_bundle_history_transform_skips_duplicate_key() {
        let existing_event = BundleHistoryEvent::Received {
            key: "duplicate-key".to_string(),
            timestamp: 1111111111,
            bundle: Box::new(create_bundle_from_txn_data()),
        };
        let bundle_history = BundleHistory { history: vec![existing_event] };

        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        let bundle_event = BundleEvent::Received { bundle_id, bundle: Box::new(bundle) };
        let event = create_test_event("duplicate-key", 1234567890, bundle_event);

        let result = update_bundle_history_transform(bundle_history, &event);

        assert!(result.is_none());
    }

    #[test]
    fn test_update_bundle_history_transform_handles_all_event_types() {
        let bundle_history = BundleHistory { history: vec![] };
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());

        let bundle_event = BundleEvent::Received { bundle_id, bundle: Box::new(bundle) };
        let event = create_test_event("test-key", 1234567890, bundle_event);
        let result = update_bundle_history_transform(bundle_history.clone(), &event);
        assert!(result.is_some());

        let bundle_event = BundleEvent::Cancelled { bundle_id };
        let event = create_test_event("test-key-2", 1234567890, bundle_event);
        let result = update_bundle_history_transform(bundle_history.clone(), &event);
        assert!(result.is_some());

        let bundle_event = BundleEvent::BuilderIncluded {
            bundle_id,
            builder: "test-builder".to_string(),
            block_number: 12345,
            flashblock_index: 1,
        };
        let event = create_test_event("test-key-3", 1234567890, bundle_event);
        let result = update_bundle_history_transform(bundle_history.clone(), &event);
        assert!(result.is_some());

        let bundle_event = BundleEvent::BlockIncluded {
            bundle_id,
            block_number: 12345,
            block_hash: TxHash::from([1u8; 32]),
        };
        let event = create_test_event("test-key-4", 1234567890, bundle_event);
        let result = update_bundle_history_transform(bundle_history.clone(), &event);
        assert!(result.is_some());

        let bundle_event = BundleEvent::Dropped { bundle_id, reason: DropReason::TimedOut };
        let event = create_test_event("test-key-5", 1234567890, bundle_event);
        let result = update_bundle_history_transform(bundle_history, &event);
        assert!(result.is_some());
    }

    #[test]
    fn test_update_transaction_metadata_transform_adds_new_bundle() {
        let metadata = TransactionMetadata { bundle_ids: vec![] };
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());

        let result = update_transaction_metadata_transform(metadata, bundle_id);

        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.bundle_ids.len(), 1);
        assert_eq!(metadata.bundle_ids[0], bundle_id);
    }

    #[test]
    fn test_update_transaction_metadata_transform_skips_existing_bundle() {
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        let metadata = TransactionMetadata { bundle_ids: vec![bundle_id] };

        let result = update_transaction_metadata_transform(metadata, bundle_id);

        assert!(result.is_none());
    }

    #[test]
    fn test_update_transaction_metadata_transform_adds_to_existing_bundles() {
        // Some different, dummy bundle IDs since create_bundle_from_txn_data() returns the same bundle ID
        // Even if the same txn is contained across multiple bundles, the bundle ID will be different since the
        // UUID is based on the bundle hash.
        let existing_bundle_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let new_bundle_id = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();

        let metadata = TransactionMetadata { bundle_ids: vec![existing_bundle_id] };

        let result = update_transaction_metadata_transform(metadata, new_bundle_id);

        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.bundle_ids.len(), 2);
        assert!(metadata.bundle_ids.contains(&existing_bundle_id));
        assert!(metadata.bundle_ids.contains(&new_bundle_id));
    }
}
