//! Admin RPC Module

use core::fmt::Debug;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_gossip::Metrics;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};
use tokio::sync::{mpsc, oneshot};

use crate::{AdminApiServer, SequencerAdminAPIClient, SequencerAdminAPIError};

/// The query types to the network actor for the admin api.
#[derive(Debug)]
pub enum NetworkAdminQuery {
    /// An admin rpc request to post an unsafe payload.
    PostUnsafePayload {
        /// The payload to post.
        payload: Box<BaseExecutionPayloadEnvelope>,
    },
    /// An admin rpc request to clear pending outbound P2P connections.
    ClearPendingP2pConnections {
        /// The response channel for the number of cleared pending connections.
        out: oneshot::Sender<usize>,
    },
}

type NetworkAdminQuerySender = mpsc::Sender<NetworkAdminQuery>;

/// The admin rpc server.
#[derive(Debug)]
pub struct AdminRpc<SequencerAdminAPIClient> {
    /// The sequencer admin API client.
    pub sequencer_admin_client: Option<SequencerAdminAPIClient>,
    /// The sender to the network actor.
    pub network_sender: NetworkAdminQuerySender,
}

impl<SequencerAdminAPIClient_> AdminRpc<SequencerAdminAPIClient_>
where
    SequencerAdminAPIClient_: SequencerAdminAPIClient,
{
    /// Constructs a new [`AdminRpc`] given the sequencer sender and network sender.
    ///
    /// # Parameters
    ///
    /// - `sequencer_sender`: The [`SequencerAdminAPIClient`] used to fulfill sequencer admin
    ///   queries.
    /// - `network_sender`: The sender to the network actor.
    ///
    /// # Returns
    ///
    /// A new [`AdminRpc`] instance.
    pub const fn new(
        sequencer_admin_client: Option<SequencerAdminAPIClient_>,
        network_sender: NetworkAdminQuerySender,
    ) -> Self {
        Self { sequencer_admin_client, network_sender }
    }
}

/// Returns an RPC error indicating the sequencer is not available on this node.
fn sequencer_unavailable() -> ErrorObject<'static> {
    ErrorObject::owned(-32001, "sequencer not available on this node", None::<()>)
}

/// Maps public sequencer admin failures without exposing internal details.
fn sequencer_admin_error(error: SequencerAdminAPIError) -> ErrorObject<'static> {
    match error {
        SequencerAdminAPIError::NotLeader => {
            ErrorObject::owned(-32002, "Node is not the conductor leader.", None::<()>)
        }
        SequencerAdminAPIError::RequestError(_)
        | SequencerAdminAPIError::ResponseError
        | SequencerAdminAPIError::ErrorAfterSequencerWasStopped(_)
        | SequencerAdminAPIError::LeaderOverrideError(_) => {
            ErrorObject::from(ErrorCode::InternalError)
        }
    }
}

#[async_trait]
impl<SequencerAdminAPIClient_> AdminApiServer for AdminRpc<SequencerAdminAPIClient_>
where
    SequencerAdminAPIClient_: SequencerAdminAPIClient + 'static + Send + Sync,
{
    async fn admin_post_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> RpcResult<()> {
        // Note: intentionally no sequencer guard here. Posting an unsafe payload is a P2P/gossip
        // operation that is valid on both sequencer and validator nodes.
        Metrics::rpc_calls("admin_postUnsafePayload").increment(1.0);
        self.network_sender
            .send(NetworkAdminQuery::PostUnsafePayload { payload: Box::new(payload) })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn admin_clear_pending_p2p_connections(&self) -> RpcResult<usize> {
        Metrics::rpc_calls("admin_clearPendingP2pConnections").increment(1.0);

        let (tx, rx) = oneshot::channel();
        self.network_sender
            .send(NetworkAdminQuery::ClearPendingP2pConnections { out: tx })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn admin_sequencer_active(&self) -> RpcResult<bool> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client
            .is_sequencer_active()
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn admin_start_sequencer(&self, unsafe_head: B256) -> RpcResult<()> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client.start_sequencer(unsafe_head).await.map_err(sequencer_admin_error)
    }

    async fn admin_stop_sequencer(&self) -> RpcResult<B256> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client.stop_sequencer().await.map_err(sequencer_admin_error)
    }

    async fn admin_conductor_enabled(&self) -> RpcResult<bool> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client
            .is_conductor_enabled()
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn admin_recover_mode(&self) -> RpcResult<bool> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client
            .is_recovery_mode()
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn admin_set_recover_mode(&self, mode: bool) -> RpcResult<()> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client
            .set_recovery_mode(mode)
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn admin_override_leader(&self) -> RpcResult<()> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client
            .override_leader()
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn admin_reset_derivation_pipeline(&self) -> RpcResult<()> {
        // If the sequencer is not enabled (mode runs in validator mode), return an error.
        let Some(ref sequencer_client) = self.sequencer_admin_client else {
            return Err(sequencer_unavailable());
        };

        sequencer_client
            .reset_derivation_pipeline()
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::types::{ErrorCode, ErrorObject};

    use super::sequencer_admin_error;
    use crate::SequencerAdminAPIError;

    #[test]
    fn sequencer_admin_error_redacts_internal_failure_details() {
        let error = sequencer_admin_error(SequencerAdminAPIError::RequestError(
            "block hash mismatch: engine unsafe head is 0x1, caller requested 0x2".to_string(),
        ));
        let internal_error = ErrorObject::from(ErrorCode::InternalError);

        assert_eq!(error.code(), internal_error.code());
        assert_eq!(error.message(), internal_error.message());
    }

    #[test]
    fn sequencer_admin_error_uses_not_leader_error() {
        let error = sequencer_admin_error(SequencerAdminAPIError::NotLeader);

        assert_eq!(error.code(), -32002);
        assert_eq!(error.message(), "Node is not the conductor leader.");
    }
}
