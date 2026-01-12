use crate::metrics::Metrics;
use crate::reader::Event;
use crate::types::{
    BundleEvent, BundleId, DropReason, TransactionId, UserOpDropReason, UserOpEvent, UserOpHash,
};
use alloy_primitives::{Address, TxHash, U256};
use anyhow::Result;
use async_trait::async_trait;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use futures::future;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::Debug;
use std::time::Instant;
use tips_core::AcceptedBundle;
use tracing::info;

/// S3 key types for storing different event types.
#[derive(Debug)]
pub enum S3Key {
    /// Key for bundle events.
    Bundle(BundleId),
    /// Key for transaction lookups by hash.
    TransactionByHash(TxHash),
    /// Key for user operation events.
    UserOp(UserOpHash),
}

impl fmt::Display for S3Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundle(bundle_id) => write!(f, "bundles/{bundle_id}"),
            Self::TransactionByHash(hash) => write!(f, "transactions/by_hash/{hash}"),
            Self::UserOp(user_op_hash) => write!(f, "userops/{user_op_hash}"),
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
            Self::Received { key, .. } => key,
            Self::Cancelled { key, .. } => key,
            Self::BuilderIncluded { key, .. } => key,
            Self::BlockIncluded { key, .. } => key,
            Self::Dropped { key, .. } => key,
        }
    }
}

/// History of events for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BundleHistory {
    /// List of history events.
    pub history: Vec<BundleHistoryEvent>,
}

/// History event for a user operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum UserOpHistoryEvent {
    /// User operation was added to the mempool.
    AddedToMempool {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Sender address.
        sender: Address,
        /// Entry point address.
        entry_point: Address,
        /// Nonce.
        nonce: U256,
    },
    /// User operation was dropped.
    Dropped {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Drop reason.
        reason: UserOpDropReason,
    },
    /// User operation was included in a block.
    Included {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Block number.
        block_number: u64,
        /// Transaction hash.
        tx_hash: TxHash,
    },
}

impl UserOpHistoryEvent {
    /// Returns the event key.
    pub fn key(&self) -> &str {
        match self {
            Self::AddedToMempool { key, .. } => key,
            Self::Dropped { key, .. } => key,
            Self::Included { key, .. } => key,
        }
    }
}

/// History of events for a user operation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserOpHistory {
    /// List of history events.
    pub history: Vec<UserOpHistoryEvent>,
}

pub(crate) use crate::reader::UserOpEventWrapper;

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
        BundleEvent::Cancelled { .. } => BundleHistoryEvent::Cancelled {
            key: event.key.clone(),
            timestamp: event.timestamp,
        },
        BundleEvent::BuilderIncluded {
            builder,
            block_number,
            flashblock_index,
            ..
        } => BundleHistoryEvent::BuilderIncluded {
            key: event.key.clone(),
            timestamp: event.timestamp,
            builder: builder.clone(),
            block_number: *block_number,
            flashblock_index: *flashblock_index,
        },
        BundleEvent::BlockIncluded {
            block_number,
            block_hash,
            ..
        } => BundleHistoryEvent::BlockIncluded {
            key: event.key.clone(),
            timestamp: event.timestamp,
            block_number: *block_number,
            block_hash: *block_hash,
        },
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

fn update_userop_history_transform(
    userop_history: UserOpHistory,
    event: &UserOpEventWrapper,
) -> Option<UserOpHistory> {
    let mut history = userop_history.history;
    let user_op_hash = event.event.user_op_hash();

    if history.iter().any(|h| h.key() == event.key) {
        info!(
            user_op_hash = %user_op_hash,
            event_key = %event.key,
            "UserOp event already exists, skipping due to deduplication"
        );
        return None;
    }

    let history_event = match &event.event {
        UserOpEvent::AddedToMempool {
            sender,
            entry_point,
            nonce,
            ..
        } => UserOpHistoryEvent::AddedToMempool {
            key: event.key.clone(),
            timestamp: event.timestamp,
            sender: *sender,
            entry_point: *entry_point,
            nonce: *nonce,
        },
        UserOpEvent::Dropped { reason, .. } => UserOpHistoryEvent::Dropped {
            key: event.key.clone(),
            timestamp: event.timestamp,
            reason: reason.clone(),
        },
        UserOpEvent::Included {
            block_number,
            tx_hash,
            ..
        } => UserOpHistoryEvent::Included {
            key: event.key.clone(),
            timestamp: event.timestamp,
            block_number: *block_number,
            tx_hash: *tx_hash,
        },
    };

    history.push(history_event);
    let userop_history = UserOpHistory { history };

    info!(
        user_op_hash = %user_op_hash,
        event_count = userop_history.history.len(),
        "Updated user op history"
    );

    Some(userop_history)
}

/// Trait for writing bundle events to storage.
#[async_trait]
pub trait EventWriter {
    /// Archives a bundle event.
    async fn archive_event(&self, event: Event) -> Result<()>;
}

/// Trait for writing user operation events to storage.
#[async_trait]
pub trait UserOpEventWriter {
    /// Archives a user operation event.
    async fn archive_userop_event(&self, event: UserOpEventWrapper) -> Result<()>;
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

/// Trait for reading user operation events from S3.
#[async_trait]
pub trait UserOpEventS3Reader {
    /// Gets the user operation history for a given hash.
    async fn get_userop_history(&self, user_op_hash: UserOpHash) -> Result<Option<UserOpHistory>>;
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
        Self {
            s3_client,
            bucket,
            metrics: Metrics::default(),
        }
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

    async fn update_userop_history(&self, event: UserOpEventWrapper) -> Result<()> {
        let s3_key = S3Key::UserOp(event.event.user_op_hash()).to_string();

        self.idempotent_write::<UserOpHistory, _>(&s3_key, |current_history| {
            update_userop_history_transform(current_history, &event)
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
            self.metrics
                .s3_get_duration
                .record(get_start.elapsed().as_secs_f64());

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
                            self.metrics
                                .s3_put_duration
                                .record(put_start.elapsed().as_secs_f64());
                            info!(
                                s3_key = %key,
                                attempt = attempt + 1,
                                "Successfully wrote object with idempotent write"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            self.metrics
                                .s3_put_duration
                                .record(put_start.elapsed().as_secs_f64());

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
        match self
            .s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
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
        let tx_futures: Vec<_> = transaction_ids
            .into_iter()
            .map(|tx_id| async move {
                self.update_transaction_by_hash_index(&tx_id, bundle_id)
                    .await
            })
            .collect();

        // Run the bundle and transaction futures concurrently and wait for them to complete
        tokio::try_join!(bundle_future, future::try_join_all(tx_futures))?;

        self.metrics
            .update_bundle_history_duration
            .record(bundle_start.elapsed().as_secs_f64());
        self.metrics
            .update_tx_indexes_duration
            .record(tx_start.elapsed().as_secs_f64());

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
        let (transaction_metadata, _) = self
            .get_object_with_etag::<TransactionMetadata>(&s3_key)
            .await?;
        Ok(transaction_metadata)
    }
}

#[async_trait]
impl UserOpEventWriter for S3EventReaderWriter {
    async fn archive_userop_event(&self, event: UserOpEventWrapper) -> Result<()> {
        self.update_userop_history(event).await
    }
}

#[async_trait]
impl UserOpEventS3Reader for S3EventReaderWriter {
    async fn get_userop_history(&self, user_op_hash: UserOpHash) -> Result<Option<UserOpHistory>> {
        let s3_key = S3Key::UserOp(user_op_hash).to_string();
        let (userop_history, _) = self.get_object_with_etag::<UserOpHistory>(&s3_key).await?;
        Ok(userop_history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::Event;
    use crate::types::{BundleEvent, DropReason, UserOpDropReason, UserOpEvent};
    use alloy_primitives::{Address, B256, TxHash, U256};
    use tips_core::{BundleExtensions, test_utils::create_bundle_from_txn_data};
    use uuid::Uuid;

    fn create_test_event(key: &str, timestamp: i64, bundle_event: BundleEvent) -> Event {
        Event {
            key: key.to_string(),
            timestamp,
            event: bundle_event,
        }
    }

    #[test]
    fn test_update_bundle_history_transform_adds_new_event() {
        let bundle_history = BundleHistory { history: vec![] };
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        let bundle_event = BundleEvent::Received {
            bundle_id,
            bundle: Box::new(bundle.clone()),
        };
        let event = create_test_event("test-key", 1234567890, bundle_event);

        let result = update_bundle_history_transform(bundle_history, &event);

        assert!(result.is_some());
        let bundle_history = result.unwrap();
        assert_eq!(bundle_history.history.len(), 1);

        match &bundle_history.history[0] {
            BundleHistoryEvent::Received {
                key,
                timestamp: ts,
                bundle: b,
            } => {
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
        let bundle_history = BundleHistory {
            history: vec![existing_event],
        };

        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        let bundle_event = BundleEvent::Received {
            bundle_id,
            bundle: Box::new(bundle),
        };
        let event = create_test_event("duplicate-key", 1234567890, bundle_event);

        let result = update_bundle_history_transform(bundle_history, &event);

        assert!(result.is_none());
    }

    #[test]
    fn test_update_bundle_history_transform_handles_all_event_types() {
        let bundle_history = BundleHistory { history: vec![] };
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());

        let bundle_event = BundleEvent::Received {
            bundle_id,
            bundle: Box::new(bundle),
        };
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

        let bundle_event = BundleEvent::Dropped {
            bundle_id,
            reason: DropReason::TimedOut,
        };
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
        let metadata = TransactionMetadata {
            bundle_ids: vec![bundle_id],
        };

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

        let metadata = TransactionMetadata {
            bundle_ids: vec![existing_bundle_id],
        };

        let result = update_transaction_metadata_transform(metadata, new_bundle_id);

        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.bundle_ids.len(), 2);
        assert!(metadata.bundle_ids.contains(&existing_bundle_id));
        assert!(metadata.bundle_ids.contains(&new_bundle_id));
    }

    fn create_test_userop_event(
        key: &str,
        timestamp: i64,
        userop_event: UserOpEvent,
    ) -> UserOpEventWrapper {
        UserOpEventWrapper {
            key: key.to_string(),
            timestamp,
            event: userop_event,
        }
    }

    #[test]
    fn test_s3_key_userop_display() {
        let hash = B256::from([1u8; 32]);
        let key = S3Key::UserOp(hash);
        let key_str = key.to_string();
        assert!(key_str.starts_with("userops/"));
        assert!(key_str.contains(&format!("{hash}")));
    }

    #[test]
    fn test_update_userop_history_transform_adds_new_event() {
        let userop_history = UserOpHistory { history: vec![] };
        let user_op_hash = B256::from([1u8; 32]);
        let sender = Address::from([2u8; 20]);
        let entry_point = Address::from([3u8; 20]);
        let nonce = U256::from(1);

        let userop_event = UserOpEvent::AddedToMempool {
            user_op_hash,
            sender,
            entry_point,
            nonce,
        };
        let event = create_test_userop_event("test-key", 1234567890, userop_event);

        let result = update_userop_history_transform(userop_history, &event);

        assert!(result.is_some());
        let history = result.unwrap();
        assert_eq!(history.history.len(), 1);

        match &history.history[0] {
            UserOpHistoryEvent::AddedToMempool {
                key,
                timestamp: ts,
                sender: s,
                entry_point: ep,
                nonce: n,
            } => {
                assert_eq!(key, "test-key");
                assert_eq!(*ts, 1234567890);
                assert_eq!(*s, sender);
                assert_eq!(*ep, entry_point);
                assert_eq!(*n, nonce);
            }
            _ => panic!("Expected AddedToMempool event"),
        }
    }

    #[test]
    fn test_update_userop_history_transform_skips_duplicate_key() {
        let user_op_hash = B256::from([1u8; 32]);
        let sender = Address::from([2u8; 20]);
        let entry_point = Address::from([3u8; 20]);
        let nonce = U256::from(1);

        let existing_event = UserOpHistoryEvent::AddedToMempool {
            key: "duplicate-key".to_string(),
            timestamp: 1111111111,
            sender,
            entry_point,
            nonce,
        };
        let userop_history = UserOpHistory {
            history: vec![existing_event],
        };

        let userop_event = UserOpEvent::AddedToMempool {
            user_op_hash,
            sender,
            entry_point,
            nonce,
        };
        let event = create_test_userop_event("duplicate-key", 1234567890, userop_event);

        let result = update_userop_history_transform(userop_history, &event);

        assert!(result.is_none());
    }

    #[test]
    fn test_update_userop_history_transform_handles_dropped_event() {
        let userop_history = UserOpHistory { history: vec![] };
        let user_op_hash = B256::from([1u8; 32]);
        let reason = UserOpDropReason::Expired;

        let userop_event = UserOpEvent::Dropped {
            user_op_hash,
            reason: reason.clone(),
        };
        let event = create_test_userop_event("dropped-key", 1234567890, userop_event);

        let result = update_userop_history_transform(userop_history, &event);

        assert!(result.is_some());
        let history = result.unwrap();
        assert_eq!(history.history.len(), 1);

        match &history.history[0] {
            UserOpHistoryEvent::Dropped {
                key,
                timestamp,
                reason: r,
            } => {
                assert_eq!(key, "dropped-key");
                assert_eq!(*timestamp, 1234567890);
                match r {
                    UserOpDropReason::Expired => {}
                    _ => panic!("Expected Expired reason"),
                }
            }
            _ => panic!("Expected Dropped event"),
        }
    }

    #[test]
    fn test_update_userop_history_transform_handles_included_event() {
        let userop_history = UserOpHistory { history: vec![] };
        let user_op_hash = B256::from([1u8; 32]);
        let tx_hash = TxHash::from([4u8; 32]);
        let block_number = 12345u64;

        let userop_event = UserOpEvent::Included {
            user_op_hash,
            block_number,
            tx_hash,
        };
        let event = create_test_userop_event("included-key", 1234567890, userop_event);

        let result = update_userop_history_transform(userop_history, &event);

        assert!(result.is_some());
        let history = result.unwrap();
        assert_eq!(history.history.len(), 1);

        match &history.history[0] {
            UserOpHistoryEvent::Included {
                key,
                timestamp,
                block_number: bn,
                tx_hash: th,
            } => {
                assert_eq!(key, "included-key");
                assert_eq!(*timestamp, 1234567890);
                assert_eq!(*bn, 12345);
                assert_eq!(*th, tx_hash);
            }
            _ => panic!("Expected Included event"),
        }
    }

    #[test]
    fn test_update_userop_history_transform_handles_all_event_types() {
        let userop_history = UserOpHistory { history: vec![] };
        let user_op_hash = B256::from([1u8; 32]);
        let sender = Address::from([2u8; 20]);
        let entry_point = Address::from([3u8; 20]);
        let nonce = U256::from(1);

        let userop_event = UserOpEvent::AddedToMempool {
            user_op_hash,
            sender,
            entry_point,
            nonce,
        };
        let event = create_test_userop_event("key-1", 1234567890, userop_event);
        let result = update_userop_history_transform(userop_history.clone(), &event);
        assert!(result.is_some());

        let userop_event = UserOpEvent::Dropped {
            user_op_hash,
            reason: UserOpDropReason::Invalid("test error".to_string()),
        };
        let event = create_test_userop_event("key-2", 1234567891, userop_event);
        let result = update_userop_history_transform(userop_history.clone(), &event);
        assert!(result.is_some());

        let userop_event = UserOpEvent::Included {
            user_op_hash,
            block_number: 12345,
            tx_hash: TxHash::from([4u8; 32]),
        };
        let event = create_test_userop_event("key-3", 1234567892, userop_event);
        let result = update_userop_history_transform(userop_history, &event);
        assert!(result.is_some());
    }

    #[test]
    fn test_userop_history_event_key_accessor() {
        let sender = Address::from([2u8; 20]);
        let entry_point = Address::from([3u8; 20]);
        let nonce = U256::from(1);

        let event1 = UserOpHistoryEvent::AddedToMempool {
            key: "key-1".to_string(),
            timestamp: 1234567890,
            sender,
            entry_point,
            nonce,
        };
        assert_eq!(event1.key(), "key-1");

        let event2 = UserOpHistoryEvent::Dropped {
            key: "key-2".to_string(),
            timestamp: 1234567890,
            reason: UserOpDropReason::Expired,
        };
        assert_eq!(event2.key(), "key-2");

        let event3 = UserOpHistoryEvent::Included {
            key: "key-3".to_string(),
            timestamp: 1234567890,
            block_number: 12345,
            tx_hash: TxHash::from([4u8; 32]),
        };
        assert_eq!(event3.key(), "key-3");
    }

    #[test]
    fn test_userop_history_serialization() {
        let sender = Address::from([2u8; 20]);
        let entry_point = Address::from([3u8; 20]);
        let nonce = U256::from(1);

        let history = UserOpHistory {
            history: vec![UserOpHistoryEvent::AddedToMempool {
                key: "test-key".to_string(),
                timestamp: 1234567890,
                sender,
                entry_point,
                nonce,
            }],
        };

        let json = serde_json::to_string(&history).unwrap();
        let deserialized: UserOpHistory = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.history.len(), 1);
        assert_eq!(deserialized.history[0].key(), "test-key");
    }
}
