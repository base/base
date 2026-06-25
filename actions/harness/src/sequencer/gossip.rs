use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_node::{UnsafePayloadGossipClient, UnsafePayloadGossipClientError};

/// No-op gossip adapter used by the actor; tests still inject gossip explicitly.
#[derive(Debug, Clone, Default)]
pub struct ActionUnsafePayloadGossipClient;

#[async_trait]
impl UnsafePayloadGossipClient for ActionUnsafePayloadGossipClient {
    async fn schedule_execution_payload_gossip(
        &self,
        _payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        Ok(())
    }
}
