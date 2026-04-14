//! RPC server for the audit archiver.
//!
//! Exposes the `base_persistRejectedTransactionBatch` method for receiving batches
//! of rejected transactions from the builder and persisting them to S3.

use base_bundles::RejectedTransaction;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use tracing::{error, info};

use crate::storage::S3EventReaderWriter;

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
}

/// RPC handler for audit archiver requests.
#[derive(Debug)]
pub struct AuditArchiverRpc {
    storage: S3EventReaderWriter,
}

impl AuditArchiverRpc {
    /// Creates a new `AuditArchiverRpc`.
    pub const fn new(storage: S3EventReaderWriter) -> Self {
        Self { storage }
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
        let block_number = batch.first().map(|tx| tx.block_number).unwrap_or(0);

        info!(batch_size, block_number, "Persisting rejected transaction batch");

        let mut persisted = 0u32;
        for rejected_tx in batch {
            if let Err(e) = self.storage.store_rejected_transaction(&rejected_tx).await {
                error!(
                    error = %e,
                    tx_hash = %rejected_tx.tx_hash,
                    "Failed to persist rejected transaction"
                );
            } else {
                persisted += 1;
            }
        }

        Ok(persisted)
    }
}
