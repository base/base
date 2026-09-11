//! S3 storage for rejected transaction records.

use anyhow::Result;
use aws_sdk_s3::{Client, primitives::ByteStream};
use base_common_types_payload::RejectedTransaction;

/// Stores rejected transaction records in S3.
#[derive(Clone, Debug)]
pub struct RejectedTransactionStore {
    client: Client,
    bucket: String,
}

impl RejectedTransactionStore {
    /// Creates a store for the given bucket.
    pub const fn new(client: Client, bucket: String) -> Self {
        Self { client, bucket }
    }

    /// Persists a rejected transaction under its block number and transaction hash.
    pub async fn store_rejected_transaction(&self, tx: &RejectedTransaction) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(format!("rejected/{}/{}", tx.block_number, tx.tx_hash))
            .body(ByteStream::from(serde_json::to_vec(tx)?))
            .send()
            .await?;
        Ok(())
    }
}
