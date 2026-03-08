use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, Bytes};
use alloy_provider::Provider;
use async_trait::async_trait;
use backon::Retryable;
use base_enclave::ExecutionWitness;
use base_proof_rpc::{L2Client, RpcError, RpcResult};

use super::prover_l2_client::ProverL2Provider;

fn truncate_error_message(message: &str, max_chars: usize) -> String {
    if message.len() <= max_chars {
        return message.to_string();
    }
    let end = message.floor_char_boundary(max_chars);
    format!("{}... (truncated)", &message[..end])
}

fn normalize_execution_witness_error(block_number: u64, error: RpcError) -> RpcError {
    if error.is_retryable() {
        return error;
    }
    let message = truncate_error_message(&error.to_string(), 500);
    RpcError::WitnessNotFound(format!("Block {block_number}: {message}"))
}

fn normalize_db_get_error(key: B256, error: RpcError) -> RpcError {
    if error.is_retryable() {
        return error;
    }
    RpcError::InvalidResponse(format!("Failed to db_get key {key}: {error}"))
}

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
        .map_err(|error| normalize_execution_witness_error(block_number, error))
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
        .map_err(|error| normalize_db_get_error(key, error))
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_db_get_error, normalize_execution_witness_error};
    use alloy_primitives::B256;
    use base_proof_rpc::RpcError;

    #[test]
    fn test_normalize_execution_witness_error_preserves_retryable() {
        let error = RpcError::Transport("temporary network error".into());
        let normalized = normalize_execution_witness_error(42, error);
        assert!(matches!(normalized, RpcError::Transport(_)));
    }

    #[test]
    fn test_normalize_execution_witness_error_wraps_non_retryable() {
        let error = RpcError::InvalidResponse("rpc returned malformed json".into());
        let normalized = normalize_execution_witness_error(42, error);
        match normalized {
            RpcError::WitnessNotFound(message) => {
                assert!(message.contains("Block 42"));
            }
            other => panic!("expected WitnessNotFound, got {other}"),
        }
    }

    #[test]
    fn test_normalize_db_get_error_preserves_retryable() {
        let key = B256::repeat_byte(0xAB);
        let error = RpcError::Timeout("request timed out".into());
        let normalized = normalize_db_get_error(key, error);
        assert!(matches!(normalized, RpcError::Timeout(_)));
    }

    #[test]
    fn test_normalize_db_get_error_wraps_non_retryable() {
        let key = B256::repeat_byte(0xCD);
        let error = RpcError::InvalidResponse("unexpected rpc payload".into());
        let normalized = normalize_db_get_error(key, error);
        assert!(matches!(normalized, RpcError::InvalidResponse(_)));
    }
}
