//! RPC server for the audit archiver.
//!
//! Exposes the `base_persistRejectedTransactionBatch` method for receiving batches
//! of rejected transactions from the builder and persisting them to S3.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use base_bundles::RejectedTransaction;
use futures::stream::{self, StreamExt};
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::error::ErrorObjectOwned};
use jsonrpsee_types::error::ErrorCode;
use moka::sync::Cache;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{metrics::Metrics, reader::Event, storage::S3EventReaderWriter, types::BundleEvent};

const MAX_BATCH_SIZE: usize = 500;

/// RPC trait for the audit archiver.
#[rpc(server, namespace = "base")]
pub trait AuditArchiverApi {
    /// Persists a batch of rejected transactions to S3 storage.
    /// Returns the number of items successfully persisted.
    #[method(name = "persistRejectedTransactionBatch")]
    async fn persist_rejected_transaction_batch(
        &self,
        batch: Vec<RejectedTransaction>,
    ) -> RpcResult<u32>;

    /// Forwards a batch of bundle lifecycle events to the in-process event
    /// reader for archival, deduplicating by `BundleEvent::generate_event_key`
    /// using an in-memory LRU cache. Returns the number of unique events
    /// forwarded (after dedup).
    #[method(name = "persistBatchedBundleEvent")]
    async fn persist_batched_bundle_event(&self, batch: Vec<BundleEvent>) -> RpcResult<u32>;
}

/// RPC handler for audit archiver requests.
#[derive(Debug)]
pub struct AuditArchiverRpc {
    storage: Arc<S3EventReaderWriter>,
    bundle_events: Option<BundleEventForwarder>,
}

/// In-memory dedup + forwarding pipeline for bundle events received over RPC.
///
/// Uses [`moka::sync::Cache`] intentionally: `forward_batch` is synchronous
/// (no `.await`), so the sync variant avoids async overhead on every
/// get/insert in the hot path. Eviction housekeeping runs inline but is
/// bounded and fast at the configured capacity.
#[derive(Debug, Clone)]
struct BundleEventForwarder {
    cache: Cache<String, ()>,
    event_tx: mpsc::Sender<Event>,
}

impl BundleEventForwarder {
    /// Forwards a batch of bundle events into the event channel, deduplicating
    /// against `self.cache`. Returns the count of unique events forwarded.
    fn forward_batch(&self, batch: Vec<BundleEvent>, timestamp: i64) -> u32 {
        let mut forwarded: u32 = 0;
        for bundle_event in batch {
            let key = bundle_event.generate_event_key();

            // Best-effort dedup: concurrent RPC calls for the same key can
            // both miss the cache (TOCTOU), but S3 enforces final dedup via
            // conditional PUTs, so duplicates here are harmless.
            if self.cache.get(&key).is_some() {
                Metrics::rpc_cache_hits().increment(1);
                continue;
            }
            Metrics::rpc_cache_misses().increment(1);

            let event = Event { key: key.clone(), event: bundle_event, timestamp };
            match self.event_tx.try_send(event) {
                Ok(()) => {
                    // Insert after successful send so a Full/Closed drop
                    // doesn't poison the cache — the event can be retried on
                    // a later RPC call. Worst case: a concurrent call also
                    // forwards the same key; S3 dedup handles that.
                    self.cache.insert(key, ());
                    forwarded += 1;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(key = %key, "audit event channel full; dropping event");
                    Metrics::rpc_channel_send_failures("full").increment(1);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    error!("audit event channel closed; dropping batch remainder");
                    Metrics::rpc_channel_send_failures("closed").increment(1);
                    break;
                }
            }
        }
        forwarded
    }
}

impl AuditArchiverRpc {
    /// Creates a new `AuditArchiverRpc` that only handles rejected-transaction
    /// batches. The bundle-event RPC method will return an error.
    pub const fn new(storage: Arc<S3EventReaderWriter>) -> Self {
        Self { storage, bundle_events: None }
    }

    /// Creates a new `AuditArchiverRpc` configured to also accept bundle-event
    /// batches over RPC, deduplicating via `cache` and forwarding unique
    /// events to `event_tx`.
    pub const fn with_bundle_events(
        storage: Arc<S3EventReaderWriter>,
        cache: Cache<String, ()>,
        event_tx: mpsc::Sender<Event>,
    ) -> Self {
        Self { storage, bundle_events: Some(BundleEventForwarder { cache, event_tx }) }
    }
}

#[async_trait::async_trait]
impl AuditArchiverApiServer for AuditArchiverRpc {
    async fn persist_rejected_transaction_batch(
        &self,
        batch: Vec<RejectedTransaction>,
    ) -> RpcResult<u32> {
        if batch.is_empty() {
            return Ok(0);
        }

        let batch_size = batch.len();
        if batch_size > MAX_BATCH_SIZE {
            return Err(ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("Batch size {batch_size} exceeds maximum of {MAX_BATCH_SIZE}"),
                None::<()>,
            ));
        }

        let block_number = batch.first().map(|tx| tx.block_number).unwrap_or(0);

        info!(batch_size, block_number, "Persisting rejected transaction batch");

        // Clone the Arc to release the borrow on `&self` so the jsonrpsee server can dispatch
        // additional concurrent batch RPC calls while this batch's S3 writes are in flight.
        let storage = Arc::clone(&self.storage);

        // Peform the S3 operations in parallel on the batch. Up to 5 concurrent operations at a time.
        let persisted = stream::iter(batch)
            .map(move |tx| {
                let storage = Arc::clone(&storage);
                async move {
                    let result = storage.store_rejected_transaction(&tx).await;
                    (tx, result)
                }
            })
            .buffer_unordered(5)
            .fold(0u32, |persisted, (tx, result)| async move {
                if let Err(e) = result {
                    error!(
                        error = %e,
                        tx_hash = %tx.tx_hash,
                        "Failed to persist rejected transaction"
                    );
                    persisted
                } else {
                    persisted + 1
                }
            })
            .await;

        Ok(persisted)
    }

    async fn persist_batched_bundle_event(&self, batch: Vec<BundleEvent>) -> RpcResult<u32> {
        if batch.is_empty() {
            return Ok(0);
        }

        let batch_size = batch.len();
        if batch_size > MAX_BATCH_SIZE {
            return Err(ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("Batch size {batch_size} exceeds maximum of {MAX_BATCH_SIZE}"),
                None::<()>,
            ));
        }

        let forwarder = self.bundle_events.as_ref().ok_or_else(|| {
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "Bundle event forwarding not configured on this audit archiver",
                None::<()>,
            )
        })?;

        let timestamp =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;

        let forwarded = forwarder.forward_batch(batch, timestamp);

        debug!(batch_size, forwarded, "Forwarded bundle event batch");
        Ok(forwarded)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base_bundles::{BundleExtensions, test_utils::create_bundle_from_txn_data};
    use moka::sync::Cache;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;

    fn build_forwarder(channel_capacity: usize) -> (BundleEventForwarder, mpsc::Receiver<Event>) {
        let cache: Cache<String, ()> =
            Cache::builder().max_capacity(1024).time_to_live(Duration::from_secs(60)).build();
        let (tx, rx) = mpsc::channel(channel_capacity);
        (BundleEventForwarder { cache, event_tx: tx }, rx)
    }

    fn received_event() -> BundleEvent {
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        BundleEvent::Received { bundle_id, bundle: Box::new(bundle) }
    }

    #[tokio::test]
    async fn forward_batch_dedupes_within_a_single_batch() {
        let (forwarder, mut rx) = build_forwarder(8);
        let event = received_event();
        let batch = vec![event.clone(), event.clone(), event];

        let forwarded = forwarder.forward_batch(batch, 0);

        assert_eq!(forwarded, 1, "duplicates within one batch should collapse to a single forward");
        let first = rx.recv().await.expect("first event should arrive");
        assert!(rx.try_recv().is_err(), "no more events should be forwarded");
        assert!(first.key.starts_with("received-"), "key should be derived from bundle hash");
    }

    #[tokio::test]
    async fn forward_batch_dedupes_across_batches() {
        let (forwarder, mut rx) = build_forwarder(8);
        let event = received_event();

        let first = forwarder.forward_batch(vec![event.clone()], 0);
        let second = forwarder.forward_batch(vec![event], 0);

        assert_eq!(first, 1, "first occurrence should forward");
        assert_eq!(second, 0, "second occurrence should be dropped by cache");
        rx.recv().await.expect("exactly one event should land");
        assert!(rx.try_recv().is_err(), "no second event should land");
    }

    #[tokio::test]
    async fn forward_batch_propagates_unique_events() {
        let (forwarder, mut rx) = build_forwarder(8);
        let bundle_a = create_bundle_from_txn_data();
        let id_a = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle_a.bundle_hash().as_slice());
        let event_a = BundleEvent::Received { bundle_id: id_a, bundle: Box::new(bundle_a) };
        let event_b = BundleEvent::Cancelled { bundle_id: Uuid::new_v4() };

        let forwarded = forwarder.forward_batch(vec![event_a, event_b], 12345);

        assert_eq!(forwarded, 2, "two distinct events should both forward");
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(first.timestamp, 12345, "timestamp should propagate");
        assert_eq!(second.timestamp, 12345, "timestamp should propagate");
    }

    #[tokio::test]
    async fn forward_batch_stops_on_closed_channel() {
        let (forwarder, rx) = build_forwarder(8);
        drop(rx);

        let event_a = received_event();
        let event_b = BundleEvent::Cancelled { bundle_id: Uuid::new_v4() };
        let forwarded = forwarder.forward_batch(vec![event_a, event_b], 0);

        assert_eq!(forwarded, 0, "closed channel should forward zero events");
    }
}
