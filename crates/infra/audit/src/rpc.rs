//! RPC server for the audit archiver.
//!
//! Exposes the `base_persistRejectedTransactionBatch` method for receiving batches
//! of rejected transactions from the builder and persisting them to S3.

use std::sync::Arc;

use base_common_types_payload::RejectedTransaction;
use futures::stream::{self, StreamExt};
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::error::ErrorObjectOwned};
use jsonrpsee_types::error::ErrorCode;
use tracing::{error, info};

use crate::{
    DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT, PgTransactionEventSink, RejectedTransactionEventQuery,
    RejectedTransactionStore, TransactionEventRecord,
};

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

    /// Returns Postgres-backed transaction event history for one transaction hash.
    #[method(name = "getTransactionEventsByHash")]
    async fn get_transaction_events_by_hash(
        &self,
        tx_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>>;

    /// Returns Postgres-backed transaction/block event history for one block number.
    #[method(name = "getTransactionEventsByBlockNumber")]
    async fn get_transaction_events_by_block_number(
        &self,
        block_number: u64,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>>;

    /// Returns Postgres-backed transaction/block event history for one block hash.
    #[method(name = "getTransactionEventsByBlockHash")]
    async fn get_transaction_events_by_block_hash(
        &self,
        block_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>>;

    /// Returns rejected transaction events by block/time range.
    #[method(name = "getRejectedTransactionEvents")]
    async fn get_rejected_transaction_events(
        &self,
        query: RejectedTransactionEventQuery,
    ) -> RpcResult<Vec<TransactionEventRecord>>;
}

/// RPC handler for audit archiver requests.
#[derive(Debug)]
pub struct AuditArchiverRpc {
    storage: Arc<RejectedTransactionStore>,
    transaction_events: Option<PgTransactionEventSink>,
}

impl AuditArchiverRpc {
    /// Creates an RPC handler for rejected-transaction batches.
    pub const fn new(storage: Arc<RejectedTransactionStore>) -> Self {
        Self { storage, transaction_events: None }
    }

    /// Attaches the Postgres transaction event store used by read APIs.
    pub fn with_transaction_event_store(mut self, store: PgTransactionEventSink) -> Self {
        self.transaction_events = Some(store);
        self
    }

    fn transaction_event_store(&self) -> RpcResult<&PgTransactionEventSink> {
        self.transaction_events.as_ref().ok_or_else(|| {
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "Transaction event Postgres store is not configured on this audit archiver",
                None::<()>,
            )
        })
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

    async fn get_transaction_events_by_hash(
        &self,
        tx_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_event_store()?
            .events_by_transaction_hash(
                &tx_hash,
                limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT),
            )
            .await
            .map_err(internal_rpc_error)
    }

    async fn get_transaction_events_by_block_number(
        &self,
        block_number: u64,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_event_store()?
            .events_by_block_number(
                block_number,
                limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT),
            )
            .await
            .map_err(internal_rpc_error)
    }

    async fn get_transaction_events_by_block_hash(
        &self,
        block_hash: String,
        limit: Option<i64>,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_event_store()?
            .events_by_block_hash(
                &block_hash,
                limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT),
            )
            .await
            .map_err(internal_rpc_error)
    }

    async fn get_rejected_transaction_events(
        &self,
        query: RejectedTransactionEventQuery,
    ) -> RpcResult<Vec<TransactionEventRecord>> {
        self.transaction_event_store()?
            .rejected_transaction_events(query)
            .await
            .map_err(internal_rpc_error)
    }
}

fn internal_rpc_error(error: anyhow::Error) -> ErrorObjectOwned {
    error!(error = %error, "transaction event query failed");
    ErrorObjectOwned::owned(
        ErrorCode::InternalError.code(),
        "internal server error".to_string(),
        None::<()>,
    )
}
