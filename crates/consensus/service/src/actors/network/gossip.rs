use std::fmt::Debug;

use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use thiserror::Error;
use tokio::sync::mpsc;

/// Client used to schedule unsafe [`BaseExecutionPayloadEnvelope`] to be gossiped.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait UnsafePayloadGossipClient: Send + Sync + Debug {
    /// This is a fire-and-forget function that schedules the provided
    /// [`BaseExecutionPayloadEnvelope`] to be gossiped. The implementation should return as
    /// quickly as possible and offers no guarantees that the payload actually was gossiped
    /// successfully.
    async fn schedule_execution_payload_gossip(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError>;
}

/// Errors that can occur when using the [`UnsafePayloadGossipClient`].
#[derive(Debug, Error)]
pub enum UnsafePayloadGossipClientError {
    /// Error sending request.
    #[error("Error sending request: {0}")]
    RequestError(String),
}

/// Queued implementation of [`UnsafePayloadGossipClient`] that relays payloads to the network
/// actor for gossip.
#[derive(Debug, Clone)]
pub struct QueuedUnsafePayloadGossipClient {
    /// Queue used to relay unsafe payloads to gossip.
    request_tx: mpsc::Sender<BaseExecutionPayloadEnvelope>,
}

impl QueuedUnsafePayloadGossipClient {
    /// Creates a payload gossip client backed by the network actor.
    pub const fn new(request_tx: mpsc::Sender<BaseExecutionPayloadEnvelope>) -> Self {
        Self { request_tx }
    }
}

#[async_trait]
impl UnsafePayloadGossipClient for QueuedUnsafePayloadGossipClient {
    async fn schedule_execution_payload_gossip(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        self.request_tx.send(payload).await.map_err(|send_error| {
            let err = UnsafePayloadGossipClientError::RequestError(
                "request channel closed".to_string(),
            );
            error!(target: "gossip_client", payload = ?send_error.0, ?err, "failed to request to gossip payload.");
            err
        })
    }
}

/// No-op gossip client for nodes that seal payloads privately without a network actor.
#[derive(Debug, Clone)]
pub struct PrivateGossipClient;

#[async_trait]
impl UnsafePayloadGossipClient for PrivateGossipClient {
    async fn schedule_execution_payload_gossip(
        &self,
        _payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        Ok(())
    }
}

#[async_trait]
impl UnsafePayloadGossipClient for Box<dyn UnsafePayloadGossipClient> {
    async fn schedule_execution_payload_gossip(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        self.as_ref().schedule_execution_payload_gossip(payload).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};

    use super::{PrivateGossipClient, UnsafePayloadGossipClient};

    fn dummy_envelope() -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash: B256::ZERO,
                transactions: vec![],
            }),
            parent_beacon_block_root: None,
        }
    }

    #[tokio::test]
    async fn private_client_gossip_is_a_no_op() {
        let client = PrivateGossipClient;

        let result = client.schedule_execution_payload_gossip(dummy_envelope()).await;

        assert!(result.is_ok());
    }
}
