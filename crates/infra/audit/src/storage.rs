use std::{fmt, fmt::Debug, time::Instant};

use alloy_primitives::TxHash;
use anyhow::Result;
use async_trait::async_trait;
use aws_sdk_s3::{
    Client as S3Client,
    error::SdkError,
    operation::{
        get_object::GetObjectError, list_objects_v2::ListObjectsV2Output,
        put_object::PutObjectError,
    },
    primitives::ByteStream,
};
use base_bundles::{AcceptedBundle, BundleExtensions, RejectedTransaction};
use futures::future;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{
    metrics::Metrics,
    reader::Event,
    types::{BundleEvent, DropReason, TransactionId},
};

/// S3 key types for storing different event types.
#[derive(Debug)]
pub enum S3Key {
    /// Key for transaction lookups by hash.
    TransactionByHash(TxHash),
    /// Key for rejected transaction storage.
    Rejected(u64, TxHash),
}

impl fmt::Display for S3Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionByHash(hash) => write!(f, "transactions/by_hash/{hash}"),
            Self::Rejected(block_number, tx_hash) => {
                write!(f, "rejected/{block_number}/{tx_hash}")
            }
        }
    }
}

/// Metadata for a transaction, tracking which bundles it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransactionMetadata {
    /// Bundle identifiers that contain this transaction.
    ///
    /// Stored as strings for backwards compatibility — old S3 objects contain
    /// UUIDs, new objects contain `B256` hex hashes.
    pub bundle_ids: Vec<String>,
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

fn to_history_event(event: &Event) -> BundleHistoryEvent {
    match &event.event {
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
    }
}

fn update_transaction_metadata_transform(
    transaction_metadata: TransactionMetadata,
    bundle_key: String,
) -> Option<TransactionMetadata> {
    let mut bundle_ids = transaction_metadata.bundle_ids;

    if bundle_ids.contains(&bundle_key) {
        return None;
    }

    bundle_ids.push(bundle_key);
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
    /// Gets the bundle history by its S3 key suffix (`bundle_hash` or `bundle_id`).
    async fn get_bundle_history(&self, bundle_key: &str) -> Result<Option<BundleHistory>>;
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
}

impl S3EventReaderWriter {
    /// Creates a new S3 event reader/writer.
    pub const fn new(s3_client: S3Client, bucket: String) -> Self {
        Self { s3_client, bucket }
    }

    /// Stores a rejected transaction to S3.
    pub async fn store_rejected_transaction(
        &self,
        rejected_tx: &RejectedTransaction,
    ) -> Result<()> {
        let s3_key = S3Key::Rejected(rejected_tx.block_number, rejected_tx.tx_hash).to_string();
        let content = serde_json::to_string(rejected_tx)?;
        self.s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(ByteStream::from(content.into_bytes()))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store rejected transaction: {e}"))?;
        Ok(())
    }

    /// Retrieves a rejected transaction from S3.
    pub async fn get_rejected_transaction(
        &self,
        block_number: u64,
        tx_hash: TxHash,
    ) -> Result<Option<RejectedTransaction>> {
        let s3_key = S3Key::Rejected(block_number, tx_hash).to_string();
        let (rejected_tx, _) = self.get_object_with_etag::<RejectedTransaction>(&s3_key).await?;
        Ok(rejected_tx)
    }

    /// Writes a single event as a standalone S3 object using `If-None-Match: *`.
    ///
    /// If the object already exists (412), another writer succeeded first — return Ok.
    async fn write_event(&self, event: &Event) -> Result<()> {
        let s3_key = event.event.s3_event_key();
        let history_event = to_history_event(event);
        let content = serde_json::to_string(&history_event)?;

        let put_request = self
            .s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(ByteStream::from(content.into_bytes()))
            .if_none_match("*");

        let put_start = Instant::now();
        match put_request.send().await {
            Ok(_) => {
                Metrics::s3_put_duration().record(put_start.elapsed().as_secs_f64());
                debug!(s3_key = %s3_key, "wrote event to S3");
                Ok(())
            }
            Err(ref e) if Self::is_conditional_write_conflict(e) => {
                Metrics::s3_put_duration().record(put_start.elapsed().as_secs_f64());
                Metrics::s3_conditional_conflicts().increment(1);
                debug!(s3_key = %s3_key, "event already exists in S3, skipping");
                Ok(())
            }
            Err(e) => {
                // TODO: retry with exponential backoff
                Metrics::s3_put_duration().record(put_start.elapsed().as_secs_f64());
                Err(anyhow::anyhow!("failed to write event to S3: {e}"))
            }
        }
    }

    async fn update_transaction_by_hash_index(
        &self,
        tx_id: &TransactionId,
        bundle_key: String,
    ) -> Result<()> {
        let s3_key = S3Key::TransactionByHash(tx_id.hash);
        let key = s3_key.to_string();

        self.idempotent_write::<TransactionMetadata, _>(&key, |current_metadata| {
            update_transaction_metadata_transform(current_metadata, bundle_key.clone())
        })
        .await
    }

    /// Returns true if the error is a conditional write conflict (412 or 409).
    ///
    /// S3 returns 412 Precondition Failed when `If-Match` / `If-None-Match` conditions
    /// are not met, and 409 Conditional Request Conflict for concurrent writes. In both
    /// cases another writer already succeeded, so the caller can re-read and skip.
    fn is_conditional_write_conflict(err: &SdkError<PutObjectError>) -> bool {
        match err {
            SdkError::ServiceError(service_err) => {
                matches!(
                    service_err.err().meta().code(),
                    Some("PreconditionFailed" | "ConditionalRequestConflict")
                )
            }
            SdkError::ResponseError(resp) => {
                let status = resp.raw().status().as_u16();
                status == 412 || status == 409
            }
            _ => false,
        }
    }

    async fn idempotent_write<T, F>(&self, key: &str, mut transform_fn: F) -> Result<()>
    where
        T: for<'de> Deserialize<'de> + Serialize + Default + Debug,
        F: FnMut(T) -> Option<T>,
    {
        const MAX_RETRIES: usize = 5;
        const BASE_DELAY_MS: u64 = 100;

        for attempt in 0..MAX_RETRIES {
            let get_start = Instant::now();
            let (current_value, etag) = self.get_object_with_etag::<T>(key).await?;
            Metrics::s3_get_duration().record(get_start.elapsed().as_secs_f64());

            let value = current_value.unwrap_or_default();

            match transform_fn(value) {
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
                            Metrics::s3_put_duration().record(put_start.elapsed().as_secs_f64());
                            debug!(
                                s3_key = %key,
                                attempt = attempt + 1,
                                "Successfully wrote object with idempotent write"
                            );
                            return Ok(());
                        }
                        Err(ref e) if Self::is_conditional_write_conflict(e) => {
                            Metrics::s3_put_duration().record(put_start.elapsed().as_secs_f64());
                            Metrics::s3_conditional_conflicts().increment(1);
                            debug!(
                                s3_key = %key,
                                attempt = attempt + 1,
                                "Conditional write conflict, another writer succeeded"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            Metrics::s3_put_duration().record(put_start.elapsed().as_secs_f64());

                            if attempt < MAX_RETRIES - 1 {
                                let delay = BASE_DELAY_MS * 2_u64.pow(attempt as u32);
                                warn!(
                                    s3_key = %key,
                                    attempt = attempt + 1,
                                    delay_ms = delay,
                                    error = %e,
                                    "S3 put failed, retrying with backoff"
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
                    Metrics::s3_writes_skipped().increment(1);
                    info!(s3_key = %key, "transform returned None, no write required");
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
                let value: T = serde_json::from_slice(&body.into_bytes())?;
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
        let bundle_key = match &event.event {
            BundleEvent::Received { bundle, .. } => format!("{}", bundle.bundle_hash()),
            // TODO: support other event types using bundle hash
            _ => anyhow::bail!("archive_event only supports Received events"),
        };
        let transaction_ids = event.event.transaction_ids();

        let event_start = Instant::now();
        let event_future = self.write_event(&event);

        let tx_start = Instant::now();
        let tx_futures: Vec<_> = transaction_ids
            .into_iter()
            .map(|tx_id| {
                let bk = bundle_key.clone();
                async move { self.update_transaction_by_hash_index(&tx_id, bk).await }
            })
            .collect();

        tokio::try_join!(event_future, future::try_join_all(tx_futures))?;

        Metrics::update_bundle_history_duration().record(event_start.elapsed().as_secs_f64());
        Metrics::update_tx_indexes_duration().record(tx_start.elapsed().as_secs_f64());

        Ok(())
    }
}

#[async_trait]
impl BundleEventS3Reader for S3EventReaderWriter {
    async fn get_bundle_history(&self, bundle_key: &str) -> Result<Option<BundleHistory>> {
        let prefix = format!("bundles/{bundle_key}/");
        let list_output: ListObjectsV2Output =
            self.s3_client.list_objects_v2().bucket(&self.bucket).prefix(&prefix).send().await?;

        let keys: Vec<String> =
            list_output.contents().iter().filter_map(|obj| obj.key().map(String::from)).collect();

        if keys.is_empty() {
            return Ok(None);
        }

        let mut history = Vec::new();
        for key in &keys {
            let (event, _) = self.get_object_with_etag::<BundleHistoryEvent>(key).await?;
            if let Some(event) = event {
                history.push(event);
            }
        }

        Ok(Some(BundleHistory { history }))
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
    use base_bundles::{BundleExtensions, test_utils::create_bundle_from_txn_data};
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
    fn test_to_history_event_received() {
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        let bundle_event = BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) };
        let event = create_test_event("test-key", 1234567890, bundle_event);

        let history_event = to_history_event(&event);

        match &history_event {
            BundleHistoryEvent::Received { key, timestamp, bundle: b } => {
                assert_eq!(key, "test-key");
                assert_eq!(*timestamp, 1234567890);
                assert_eq!(b.block_number, bundle.block_number);
            }
            _ => panic!("expected Received event"),
        }
    }

    #[test]
    fn test_to_history_event_all_types() {
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());

        let cases: Vec<(&str, BundleEvent)> = vec![
            ("received", BundleEvent::Received { bundle_id, bundle: Box::new(bundle) }),
            ("cancelled", BundleEvent::Cancelled { bundle_id }),
            (
                "builder-included",
                BundleEvent::BuilderIncluded {
                    bundle_id,
                    builder: "test-builder".to_string(),
                    block_number: 12345,
                    flashblock_index: 1,
                },
            ),
            (
                "block-included",
                BundleEvent::BlockIncluded {
                    bundle_id,
                    block_number: 12345,
                    block_hash: TxHash::from([1u8; 32]),
                },
            ),
            ("dropped", BundleEvent::Dropped { bundle_id, reason: DropReason::TimedOut }),
        ];

        for (name, bundle_event) in cases {
            let event = create_test_event(&format!("key-{name}"), 1234567890, bundle_event);
            let history_event = to_history_event(&event);
            assert_eq!(history_event.key(), format!("key-{name}"), "key mismatch for {name}");
        }
    }

    #[test]
    fn test_update_transaction_metadata_transform_adds_new_bundle() {
        let metadata = TransactionMetadata { bundle_ids: vec![] };
        let bundle = create_bundle_from_txn_data();
        let key = format!("{}", bundle.bundle_hash());

        let result = update_transaction_metadata_transform(metadata, key.clone());

        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.bundle_ids.len(), 1);
        assert_eq!(metadata.bundle_ids[0], key);
    }

    #[test]
    fn test_update_transaction_metadata_transform_skips_existing_bundle() {
        let bundle = create_bundle_from_txn_data();
        let key = format!("{}", bundle.bundle_hash());
        let metadata = TransactionMetadata { bundle_ids: vec![key.clone()] };

        let result = update_transaction_metadata_transform(metadata, key);

        assert!(result.is_none());
    }

    #[test]
    fn test_update_transaction_metadata_transform_adds_to_existing_bundles() {
        let existing = "0xaaaa".to_string();
        let new = "0xbbbb".to_string();

        let metadata = TransactionMetadata { bundle_ids: vec![existing.clone()] };

        let result = update_transaction_metadata_transform(metadata, new.clone());

        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.bundle_ids.len(), 2);
        assert!(metadata.bundle_ids.contains(&existing));
        assert!(metadata.bundle_ids.contains(&new));
    }
}
