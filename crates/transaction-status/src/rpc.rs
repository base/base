use aws_sdk_s3::Client as S3Client;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use tips_audit::{BundleEventS3Reader, BundleHistory, S3EventReaderWriter};
use tracing::info;
use uuid::Uuid;

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, id: Uuid) -> RpcResult<Option<BundleHistory>>;
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
    async fn transaction_status(&self, id: Uuid) -> RpcResult<Option<BundleHistory>> {
        info!(message = "getting bundle history", id = %id);

        let history = self.s3.get_bundle_history(id).await.map_err(|e| {
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                format!("Failed to get bundle history: {}", e),
                None::<()>,
            )
        })?;

        Ok(history)
    }
}
