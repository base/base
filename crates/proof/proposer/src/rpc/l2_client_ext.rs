use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, Bytes};
use alloy_provider::Provider;
use async_trait::async_trait;
use backon::Retryable;
use base_enclave::ExecutionWitness;
use base_proof_rpc::{L2Client, RpcError, RpcResult};

use super::prover_l2_client::ProverL2Provider;

#[async_trait]
impl ProverL2Provider for L2Client {
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness> {
        let backoff = self.retry_config().to_backoff_builder();

        (|| async {
            self.provider()
                .raw_request::<_, ExecutionWitness>(
                    "debug_executionWitness".into(),
                    (BlockNumberOrTag::Number(block_number),),
                )
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::execution_witness");
        })
        .await
        .map_err(|e| RpcError::WitnessNotFound(format!("Block {block_number}: {e}")))
    }

    async fn db_get(&self, key: B256) -> RpcResult<Bytes> {
        let backoff = self.retry_config().to_backoff_builder();

        (|| async {
            self.provider()
                .raw_request::<_, Bytes>("debug_dbGet".into(), (key,))
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::db_get");
        })
        .await
        .map_err(|e| RpcError::InvalidResponse(format!("Failed to db_get key {key}: {e}")))
    }
}
