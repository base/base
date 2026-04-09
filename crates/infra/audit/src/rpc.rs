//! RPC server for the audit archiver.
//!
//! Exposes the `base_persistTransaction` method for receiving rejected
//! transactions from the builder and persisting them to S3.

use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use tracing::{error, info};

use base_bundles::RejectedTransaction;

use crate::storage::S3EventReaderWriter;

/// RPC trait for the audit archiver.
#[rpc(server, namespace = "base")]
pub trait AuditArchiverApi {
    /// Persists a rejected transaction to S3 storage.
    #[method(name = "persistTransaction")]
    async fn persist_transaction(&self, rejected_tx: RejectedTransaction) -> RpcResult<bool>;
}

/// RPC handler for audit archiver requests.
#[derive(Debug)]
pub struct AuditArchiverRpc {
    /// S3 storage backend.
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
    async fn persist_transaction(&self, rejected_tx: RejectedTransaction) -> RpcResult<bool> {
        info!(
            tx_hash = %rejected_tx.tx_hash,
            block_number = rejected_tx.block_number,
            reason = %rejected_tx.reason,
            "Persisting rejected transaction"
        );

        self.storage.store_rejected_transaction(&rejected_tx).await.map_err(|e| {
            error!(
                error = %e,
                tx_hash = %rejected_tx.tx_hash,
                "Failed to persist rejected transaction"
            );
            jsonrpsee::types::ErrorObject::owned(
                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                "Failed to persist rejected transaction",
                Some(e.to_string()),
            )
        })?;

        Ok(true)
    }
}
