use std::fmt::Debug;

use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use tokio::sync::mpsc;

use crate::{EngineActorRequest, EngineClientError, EngineClientResult};

/// Client used to interact with the Engine.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait NetworkEngineClient: Debug + Send + Sync {
    /// Sends an unsafe block authenticated by the P2P gossip layer.
    ///
    /// Note: a successful response does not mean the block was successfully inserted.
    /// This function just sends the message to the engine. It does not wait for a response.
    async fn send_unsafe_block(
        &self,
        block: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<()>;

    /// Sends an unsafe block supplied through the admin API.
    async fn send_admin_unsafe_block(
        &self,
        block: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<()>;
}

/// Client to use to send unsafe blocks to the Engine's inbound channel.
#[derive(Debug)]
pub struct QueuedNetworkEngineClient {
    /// A channel to use to send the `EngineActor` requests.
    pub engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
}

#[async_trait]
impl NetworkEngineClient for QueuedNetworkEngineClient {
    async fn send_unsafe_block(
        &self,
        block: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<()> {
        trace!(target: "network", ?block, "Sending unsafe block to engine.");
        Ok(self
            .engine_actor_request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(block)))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?)
    }

    async fn send_admin_unsafe_block(
        &self,
        block: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<()> {
        trace!(target: "network", ?block, "Sending admin unsafe block to engine.");
        Ok(self
            .engine_actor_request_tx
            .send(EngineActorRequest::ProcessAdminUnsafeL2BlockRequest(Box::new(block)))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use tokio::sync::mpsc;

    use super::{NetworkEngineClient, QueuedNetworkEngineClient};
    use crate::EngineActorRequest;

    #[tokio::test]
    async fn queued_client_preserves_unsafe_payload_provenance() {
        let (engine_actor_request_tx, mut request_rx) = mpsc::channel(2);
        let client = QueuedNetworkEngineClient { engine_actor_request_tx };
        let payload = || BaseExecutionPayloadEnvelope {
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
        };

        client
            .send_unsafe_block(payload())
            .await
            .expect("authenticated P2P payload should be sent");
        client.send_admin_unsafe_block(payload()).await.expect("admin payload should be sent");

        assert!(matches!(
            request_rx.recv().await,
            Some(EngineActorRequest::ProcessUnsafeL2BlockRequest(_))
        ));
        assert!(matches!(
            request_rx.recv().await,
            Some(EngineActorRequest::ProcessAdminUnsafeL2BlockRequest(_))
        ));
    }
}
