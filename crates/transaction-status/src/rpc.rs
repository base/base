use alloy_primitives::TxHash;
use aws_sdk_s3::Client as S3Client;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use tips_audit::{BundleEventS3Reader, BundleHistory, S3EventReaderWriter, TransactionMetadata};
use tracing::info;

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<Option<BundleHistory>>;
}

/// Implementation of the metering RPC API
pub struct TransactionStatusApiImpl {
    s3: S3EventReaderWriter,
}

impl TransactionStatusApiImpl {
    /// Creates a new instance of TransactionStatusApi
    pub fn new(client: S3Client, bucket: String) -> Self {
        info!(message = "using aws s3 client");
        let s3 = S3EventReaderWriter::new(client, bucket);
        Self { s3 }
    }
}

#[async_trait]
impl TransactionStatusApiServer for TransactionStatusApiImpl {
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<Option<BundleHistory>> {
        info!(message = "getting bundle history", tx_hash = %tx_hash);

        let metadata: Option<TransactionMetadata> =
            self.s3.get_transaction_metadata(tx_hash).await.map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("Failed to get transaction metadata: {}", e),
                    None::<()>,
                )
            })?;

        match metadata {
            Some(metadata) => {
                // TODO: a transaction can be in multiple bundles, but for now we'll only get the latest one
                let bundle_id = metadata.bundle_ids[metadata.bundle_ids.len() - 1];
                let history = self.s3.get_bundle_history(bundle_id).await.map_err(|e| {
                    ErrorObjectOwned::owned(
                        ErrorCode::InternalError.code(),
                        format!("Failed to get bundle history: {}", e),
                        None::<()>,
                    )
                })?;
                Ok(history)
            }
            None => Ok(None),
        }
    }
}
