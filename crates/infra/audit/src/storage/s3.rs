//! S3-backed storage implementation for the audit archiver.

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
use base_bundles::{BundleExtensions, RejectedTransaction};
use futures::future;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{
    metrics::Metrics,
    reader::Event,
    storage::{
        BundleEventReader, BundleHistory, BundleHistoryEvent, EventWriter, RejectedTxStore,
        TransactionMetadata, to_history_event, update_transaction_metadata_transform,
    },
    types::{BundleEvent, TransactionId},
};

/// S3 key types for storing different event types.
#[derive(Debug)]
enum S3Key {
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

    /// Writes a single event as a standalone S3 object using `If-None-Match: *`.
    ///
    /// If the object already exists (412), another writer succeeded first -- return Ok.
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
                Metrics::storage_put_duration().record(put_start.elapsed().as_secs_f64());
                debug!(s3_key = %s3_key, "wrote event to S3");
                Ok(())
            }
            Err(ref e) if Self::is_conditional_write_conflict(e) => {
                Metrics::storage_put_duration().record(put_start.elapsed().as_secs_f64());
                Metrics::storage_conditional_conflicts().increment(1);
                debug!(s3_key = %s3_key, "event already exists in S3, skipping");
                Ok(())
            }
            Err(e) => {
                // TODO: retry with exponential backoff
                Metrics::storage_put_duration().record(put_start.elapsed().as_secs_f64());
                Err(anyhow::anyhow!("failed to write event to S3: {e}"))
            }
        }
    }

    /// Updates the transaction-by-hash index with the given bundle key.
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

    /// Performs an idempotent read-modify-write against S3 with optimistic concurrency.
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
            Metrics::storage_get_duration().record(get_start.elapsed().as_secs_f64());

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
                            Metrics::storage_put_duration()
                                .record(put_start.elapsed().as_secs_f64());
                            debug!(
                                s3_key = %key,
                                attempt = attempt + 1,
                                "Successfully wrote object with idempotent write"
                            );
                            return Ok(());
                        }
                        Err(ref e) if Self::is_conditional_write_conflict(e) => {
                            Metrics::storage_put_duration()
                                .record(put_start.elapsed().as_secs_f64());
                            Metrics::storage_conditional_conflicts().increment(1);
                            debug!(
                                s3_key = %key,
                                attempt = attempt + 1,
                                "Conditional write conflict, another writer succeeded"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            Metrics::storage_put_duration()
                                .record(put_start.elapsed().as_secs_f64());

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
                    Metrics::storage_writes_skipped().increment(1);
                    info!(s3_key = %key, "transform returned None, no write required");
                    return Ok(());
                }
            }
        }

        Err(anyhow::anyhow!("Exceeded maximum retry attempts"))
    }

    /// Fetches an object from S3, returning its deserialized value and `ETag`.
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
impl BundleEventReader for S3EventReaderWriter {
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

#[async_trait]
impl RejectedTxStore for S3EventReaderWriter {
    async fn store_rejected_transaction(&self, rejected_tx: &RejectedTransaction) -> Result<()> {
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

    async fn get_rejected_transaction(
        &self,
        block_number: u64,
        tx_hash: TxHash,
    ) -> Result<Option<RejectedTransaction>> {
        let s3_key = S3Key::Rejected(block_number, tx_hash).to_string();
        let (rejected_tx, _) = self.get_object_with_etag::<RejectedTransaction>(&s3_key).await?;
        Ok(rejected_tx)
    }
}
